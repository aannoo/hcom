#!/usr/bin/env python3
"""Shared constants and utilities for hcom"""

from __future__ import annotations

import sys
import platform
import re
import os
from pathlib import Path
from dataclasses import dataclass
from typing import Any, Final, Literal

__version__ = "0.6.20"

# ===== Platform Detection =====
IS_WINDOWS = sys.platform == "win32"
CREATE_NO_WINDOW = 0x08000000  # Windows: prevent console window creation

# ===== Terminal Identity =====


def is_wsl() -> bool:
    """Detect if running in WSL"""
    if platform.system() != "Linux":
        return False
    try:
        with open("/proc/version", "r") as f:
            return "microsoft" in f.read().lower()
    except (FileNotFoundError, PermissionError, OSError):
        return False


def is_termux() -> bool:
    """Detect if running in Termux on Android"""
    return (
        "TERMUX_VERSION" in os.environ  # Primary: Works all versions
        or "TERMUX__ROOTFS" in os.environ  # Modern: v0.119.0+
        or Path("/data/data/com.termux").exists()  # Fallback: Path check
        or "com.termux" in os.environ.get("PREFIX", "")  # Fallback: PREFIX check
    )


# Default Termux node path for shebang bypass
TERMUX_NODE_PATH = "/data/data/com.termux/files/usr/bin/node"


def termux_shebang_bypass(command: list[str], tool: str) -> list[str]:
    """Apply Termux shebang bypass for npm-installed CLI tools.

    On Termux, npm global CLIs have shebangs like #!/usr/bin/env node
    which fail because /usr/bin/env doesn't exist. This function
    rewrites the command to explicitly call node with the tool path.

    Args:
        command: Original command list, e.g. ["gemini", "--arg"]
        tool: Tool name ("gemini" or "codex")

    Returns:
        Modified command list for Termux, or original if not on Termux
        or tool not found.
    """
    import shutil

    if not is_termux():
        return command

    if not command or command[0] != tool:
        return command

    # Find tool path
    tool_path = shutil.which(tool)
    if not tool_path:
        return command  # Let it fail naturally

    # Find node path
    node_path = shutil.which("node") or TERMUX_NODE_PATH

    # Rewrite: ["gemini", ...args] -> ["node", "/path/to/gemini", ...args]
    return [node_path, tool_path] + command[1:]


def is_inside_ai_tool() -> bool:
    """Detect if running inside any AI CLI tool (Claude Code, Gemini CLI, Codex).

    Uses contextvars for daemon mode, falls back to os.environ for CLI mode.

    Returns True if:
    - CLAUDECODE=1 (running inside Claude Code - may or may not be hcom-launched)
    - HCOM_LAUNCHED=1 (Gemini/Codex are always hcom-launched; catches vanilla Claude too)
    - GEMINI_CLI=1 (running inside Gemini CLI)
    - CODEX_SANDBOX* or CODEX_MANAGED_BY_* (running inside Codex)
    """
    from .core.thread_context import get_is_claude, get_is_gemini, get_is_codex, get_is_launched

    return get_is_claude() or get_is_launched() or get_is_gemini() or get_is_codex()


def detect_current_tool() -> str:
    """Detect current AI tool environment (vanilla or hcom-launched).

    Uses contextvars for daemon mode, falls back to os.environ for CLI mode.

    Returns: 'claude', 'codex', 'gemini', or 'adhoc' if unknown.
    """
    from .core.thread_context import get_is_claude, get_is_gemini, get_is_codex

    if get_is_claude():
        return "claude"
    if get_is_codex():
        return "codex"
    if get_is_gemini():
        return "gemini"
    return "adhoc"


def detect_vanilla_tool() -> str | None:
    """Detect if running inside a vanilla (non-hcom-launched) AI tool.

    Uses contextvars for daemon mode, falls back to os.environ for CLI mode.

    Returns tool type ('claude', 'codex', 'gemini') or None if not vanilla.
    """
    from .core.thread_context import get_is_launched

    if get_is_launched():
        return None  # Launched via hcom - not vanilla
    tool = detect_current_tool()
    return tool if tool != "adhoc" else None


# ===== Release Configuration =====
# Tools available for launch (flip to enable)
RELEASED_TOOLS = {"claude", "gemini", "codex"}  # codex: coming soon
# Tools that support background/headless mode
RELEASED_BACKGROUND = {"claude"}

# ===== Environment Variable Constants =====
# Tool detection markers - set by AI tools, cleared to prevent inheritance
TOOL_MARKER_VARS = (
    "CLAUDECODE",
    "GEMINI_CLI",
    "GEMINI_SYSTEM_MD",
    "CODEX_SANDBOX",
    "CODEX_SANDBOX_NETWORK_DISABLED",
    "CODEX_MANAGED_BY_NPM",
    "CODEX_MANAGED_BY_BUN",
    "CODEX_THREAD_ID",
)

# HCOM identity vars - set per-instance, cleared to prevent parent identity leakage
# Keep in sync with FORWARD_ENV in src/native/src/client/mod.rs
HCOM_IDENTITY_VARS = (
    "HCOM_PROCESS_ID",
    "HCOM_LAUNCHED",
    "HCOM_LAUNCHED_PRESET",
    "HCOM_PTY_MODE",
    "HCOM_BACKGROUND",
    "HCOM_LAUNCHED_BY",
    "HCOM_LAUNCH_BATCH_ID",
    "HCOM_LAUNCH_EVENT_ID",
)

# ===== Message Constants =====
# Message patterns
# Negative lookbehind excludes ._- to prevent matching:
# - email addresses: user@domain.com (preceded by letter)
# - paths: /path/to/file.@test (preceded by period)
# - identifiers: var_@name (preceded by underscore)
# - kebab-case: some-id@mention (preceded by hyphen)
# Capture group must start with alphanumeric (prevents @-test, @_test, @123)
# Includes : for remote instance names (e.g., @luna:BOXE)
MENTION_PATTERN = re.compile(r"(?<![a-zA-Z0-9._-])@([a-zA-Z0-9][\w:-]*)")

# Binding marker for vanilla sessions: [hcom:<base_name>] (also matches legacy [HCOM:BIND:<base_name>])
BIND_MARKER_RE = re.compile(r"\[(?:hcom|HCOM:BIND):([a-z0-9_]+)\]")

# Sender constants
SENDER = "bigboss"  # CLI sender identity
SYSTEM_SENDER = "hcom"  # System notification identity (launcher, watchdog, etc)
SENDER_EMOJI = "🐳"  # Legacy whale, unused but kept here to remind me about cake intake
MAX_MESSAGES_PER_DELIVERY = 50
MAX_MESSAGE_SIZE = 1048576  # 1MB


# ===== Message Identity =====
@dataclass
class SenderIdentity:
    """Sender identity for message routing.

    Attributes:
        kind: Identity type that determines routing behavior:
            'instance' - A registered hcom participant (full routing rules apply).
            'external' - External sender via --from flag (broadcasts to all).
            'system' - System-generated message (broadcasts to all).
        name: Display name stored in events.instance column.
        instance_data: Full instance data dict from DB (for kind='instance' only).
        session_id: Claude session ID for transcript binding (available even when
            instance_data is None, used for group membership).
    """

    kind: Literal["external", "instance", "system"]
    name: str
    instance_data: dict[str, Any] | None = None
    session_id: str | None = None

    @property
    def broadcasts(self) -> bool:
        """External and system senders broadcast to everyone."""
        return self.kind in ("external", "system")

    @property
    def group_id(self) -> str | None:
        """Group session ID for routing (session-based group membership)."""
        from hcom.core.helpers import get_group_session_id

        return get_group_session_id(self.instance_data)


@dataclass(frozen=True)
class CommandContext:
    """Resolved identity context for a single CLI invocation.

    - explicit_name: raw `--name` value (if provided)
    - identity: resolved instance identity (best-effort; may be None)
    """

    explicit_name: str | None
    identity: SenderIdentity | None


class HcomError(Exception):
    """HCOM operation failed."""

    pass


# ===== Hook Constants =====
# Stop hook polling interval
STOP_HOOK_POLL_INTERVAL = 0.1  # 100ms between stop hook polls


# ===== ANSI Colors (inlined to avoid ui package import overhead) =====
# Core ANSI codes
RESET = "\033[0m"
DIM = "\033[2m"
BOLD = "\033[1m"
REVERSE = "\033[7m"

# Foreground colors
FG_GREEN = "\033[32m"
FG_CYAN = "\033[36m"
FG_WHITE = "\033[37m"
FG_BLACK = "\033[30m"
FG_GRAY = "\033[38;5;242m"
FG_YELLOW = "\033[33m"
FG_RED = "\033[31m"
FG_BLUE = "\033[38;5;75m"

# TUI-specific foreground
FG_ORANGE = "\033[38;5;208m"
FG_GOLD = "\033[38;5;220m"
FG_LIGHTGRAY = "\033[38;5;250m"
FG_DELIVER = "\033[38;5;156m"
FG_STALE = "\033[38;5;137m"

# Background colors
BG_BLUE = "\033[48;5;69m"
BG_GREEN = "\033[42m"
BG_CYAN = "\033[46m"
BG_YELLOW = "\033[43m"
BG_RED = "\033[41m"
BG_GRAY = "\033[100m"
BG_STALE = "\033[48;5;137m"
BG_ORANGE = "\033[48;5;208m"
BG_CHARCOAL = "\033[48;5;236m"
BG_GOLD = "\033[48;5;220m"

# Terminal control
CLEAR_SCREEN = "\033[2J"
CURSOR_HOME = "\033[H"
HIDE_CURSOR = "\033[?25l"
SHOW_CURSOR = "\033[?25h"

# Box drawing
BOX_H = "─"

# ===== Status Configuration =====
# Status values stored directly in instance files (no event mapping)
# Row exists = participating (no separate enabled field)

# Status name constants — use these instead of raw strings.
# Rust delivery.rs and db.rs also use these values (see source-of-truth comments there).
ST_ACTIVE: Final = "active"
ST_LISTENING: Final = "listening"
ST_BLOCKED: Final = "blocked"
ST_INACTIVE: Final = "inactive"
ST_LAUNCHING: Final = "launching"
ST_ERROR: Final = "error"

# Valid status values
# ST_LISTENING is special: heartbeat-proven current (refreshed every ~30s)
# Other statuses are event-based (show time since last update)
STATUS_VALUES = [ST_ACTIVE, ST_LISTENING, ST_BLOCKED, ST_LAUNCHING, ST_ERROR, ST_INACTIVE]

# Status icons
STATUS_ICONS = {
    ST_ACTIVE: "▶",
    ST_LISTENING: "◉",
    ST_BLOCKED: "■",
    ST_LAUNCHING: "◎",
    ST_ERROR: "✗",
    ST_INACTIVE: "○",
}

# Adhoc instance icon (neutral - not claiming alive or dead)
ADHOC_ICON = "◦"

# Status colors (foreground)
STATUS_COLORS = {
    ST_ACTIVE: FG_GREEN,
    ST_LISTENING: FG_BLUE,
    ST_BLOCKED: FG_RED,
    ST_LAUNCHING: FG_YELLOW,
    ST_ERROR: FG_RED,
    ST_INACTIVE: FG_GRAY,
}

# STATUS_MAP for watch command (foreground color, icon)
STATUS_MAP = {status: (STATUS_COLORS[status], STATUS_ICONS[status]) for status in STATUS_VALUES}

# Background colors for statusline display blocks
STATUS_BG_COLORS = {
    ST_ACTIVE: BG_GREEN,
    ST_LISTENING: BG_BLUE,
    ST_BLOCKED: BG_RED,
    ST_LAUNCHING: BG_YELLOW,
    ST_ERROR: BG_RED,
    ST_INACTIVE: BG_GRAY,
}

# Background color map for TUI statusline (background color, icon)
STATUS_BG_MAP = {status: (STATUS_BG_COLORS[status], STATUS_ICONS[status]) for status in STATUS_VALUES}

# Display order (priority-based sorting)
STATUS_ORDER = [ST_ACTIVE, ST_LISTENING, ST_BLOCKED, ST_ERROR, ST_LAUNCHING, ST_INACTIVE]

# TUI-specific (alias for STATUS_COLORS)
STATUS_FG = STATUS_COLORS

# ===== Terminal Presets =====
# Each preset: {binary, open, close, platforms}
# binary: executable to check for availability (None = check app bundle instead)
# open: command template with {script} placeholder
# close: command template with {pid} placeholder (None = no close API)
# platforms: list of platform.system() values
TERMINAL_PRESETS: dict[str, dict] = {
    # macOS
    "Terminal.app": {
        "binary": None,
        "open": "open -a Terminal {script}",
        "close": None,
        "platforms": ["Darwin"],
    },
    "iTerm": {
        "binary": None,
        "open": "open -a iTerm {script}",
        "close": None,
        "platforms": ["Darwin"],
    },
    "Ghostty": {
        "binary": None,
        "open": "open -na Ghostty.app --args -e bash {script}",
        "close": None,
        "platforms": ["Darwin"],
    },
    # Cross-platform terminals (on macOS, falls back to .app bundle if CLI not in PATH)
    # "kitty" and "wezterm" are smart presets: auto-detect split/tab/window in launch_terminal().
    # Use kitty-window/wezterm-window to force a new OS window.
    "kitty": {
        "binary": "kitty",
        "open": "kitty --env HCOM_PROCESS_ID={process_id} {script}",
        "close": "kitten @ close-window --match id:{pane_id}",
        "platforms": ["Darwin", "Linux"],
    },
    "kitty-window": {
        "binary": "kitty",
        "app_name": "kitty",
        "open": "kitty --env HCOM_PROCESS_ID={process_id} {script}",
        "close": "kitten @ close-window --match id:{pane_id}",
        "platforms": ["Darwin", "Linux"],
    },
    "wezterm": {
        "binary": "wezterm",
        "app_name": "WezTerm",
        "open": "wezterm start -- bash {script}",
        "close": "wezterm cli kill-pane --pane-id {pane_id}",
        "pane_id_env": "WEZTERM_PANE",
        "platforms": ["Darwin", "Linux", "Windows"],
    },
    "wezterm-window": {
        "binary": "wezterm",
        "app_name": "WezTerm",
        "open": "wezterm start -- bash {script}",
        "close": "wezterm cli kill-pane --pane-id {pane_id}",
        "pane_id_env": "WEZTERM_PANE",
        "platforms": ["Darwin", "Linux", "Windows"],
    },
    "alacritty": {
        "binary": "alacritty",
        "app_name": "Alacritty",
        "open": "alacritty -e bash {script}",
        "close": None,
        "platforms": ["Darwin", "Linux", "Windows"],
    },
    # Tab utilities
    "ttab": {
        "binary": "ttab",
        "open": "ttab {script}",
        "close": None,
        "platforms": ["Darwin"],
    },
    "wttab": {
        "binary": "wttab",
        "open": "wttab {script}",
        "close": None,
        "platforms": ["Windows"],
    },
    # Linux terminals
    "gnome-terminal": {
        "binary": "gnome-terminal",
        "open": "gnome-terminal --window -- bash {script}",
        "close": None,
        "platforms": ["Linux"],
    },
    "konsole": {
        "binary": "konsole",
        "open": "konsole -e bash {script}",
        "close": None,
        "platforms": ["Linux"],
    },
    "xterm": {
        "binary": "xterm",
        "open": "xterm -e bash {script}",
        "close": None,
        "platforms": ["Linux"],
    },
    "tilix": {
        "binary": "tilix",
        "open": "tilix -e bash {script}",
        "close": None,
        "platforms": ["Linux"],
    },
    "terminator": {
        "binary": "terminator",
        "open": "terminator -x bash {script}",
        "close": None,
        "platforms": ["Linux"],
    },
    # Windows
    "Windows Terminal": {
        "binary": "wt",
        "open": "wt bash {script}",
        "close": None,
        "platforms": ["Windows"],
    },
    "mintty": {
        "binary": "mintty",
        "open": "mintty bash {script}",
        "close": None,
        "platforms": ["Windows"],
    },
    # Within-terminal splits/tabs
    "tmux": {
        "binary": "tmux",
        "open": "tmux new-session -d bash {script}",
        "close": "tmux kill-pane -t {pane_id}",
        "pane_id_env": "TMUX_PANE",
        "platforms": ["Darwin", "Linux"],
    },
    "tmux-split": {
        "binary": "tmux",
        "open": "tmux split-window -h {script}",
        "close": "tmux kill-pane -t {pane_id}",
        "pane_id_env": "TMUX_PANE",
        "platforms": ["Darwin", "Linux"],
    },
    "wezterm-tab": {
        "binary": "wezterm",
        "app_name": "WezTerm",
        "open": "wezterm cli spawn -- bash {script}",
        "close": "wezterm cli kill-pane --pane-id {pane_id}",
        "pane_id_env": "WEZTERM_PANE",
        "platforms": ["Darwin", "Linux", "Windows"],
    },
    "wezterm-split": {
        "binary": "wezterm",
        "app_name": "WezTerm",
        "open": "wezterm cli split-pane --top-level --right -- bash {script}",
        "close": "wezterm cli kill-pane --pane-id {pane_id}",
        "pane_id_env": "WEZTERM_PANE",
        "platforms": ["Darwin", "Linux", "Windows"],
    },
    "kitty-tab": {
        "binary": "kitten",
        "app_name": "kitty",
        "open": "kitten @ launch --type=tab --env HCOM_PROCESS_ID={process_id} -- bash {script}",
        "close": "kitten @ close-tab --match id:{pane_id}",
        "platforms": ["Darwin", "Linux"],
    },
    "kitty-split": {
        "binary": "kitten",
        "app_name": "kitty",
        "open": "kitten @ launch --type=window --env HCOM_PROCESS_ID={process_id} -- bash {script}",
        "close": "kitten @ close-window --match id:{pane_id}",
        "platforms": ["Darwin", "Linux"],
    },
}

# Map environment variables to terminal presets for auto-detection.
# Used for same-terminal PTY launches (run_here=True) to enable close-on-kill.
TERMINAL_ENV_MAP: dict[str, str] = {
    "TMUX_PANE": "tmux-split",
    "WEZTERM_PANE": "wezterm-split",
    "KITTY_WINDOW_ID": "kitty-split",
    "ZELLIJ_PANE_ID": "zellij",  # TODO: no external CLI to close pane by PID
    "WAVETERM_BLOCKID": "waveterm",  # TODO: needs block_id storage, not PID-based
}


# ===== Pure Utility Functions =====
def shorten_path(path: str) -> str:
    """Shorten path by replacing home directory with ~"""
    if not path:
        return path
    return path.replace(os.path.expanduser("~"), "~")


def get_project_tag(directory: str, max_len: int = 12) -> str:
    """Extract project name from directory path, truncated to max_len.

    Returns last path component (folder name), truncated with ellipsis if needed.
    Returns empty string if directory is empty/None.
    """
    if not directory:
        return ""
    project = Path(directory).name
    if len(project) > max_len:
        project = project[: max_len - 1] + "…"
    return project


def parse_iso_timestamp(iso_str: str):
    """Parse ISO timestamp string to datetime, handling Z timezone.
    Returns datetime object or None on parse failure."""
    from datetime import datetime

    try:
        return datetime.fromisoformat(iso_str.replace("Z", "+00:00"))
    except (ValueError, AttributeError):
        return None


def format_timestamp(iso_str: str, fmt: str = "%H:%M") -> str:
    """Format ISO timestamp for display in local time."""
    try:
        if "T" in iso_str:
            dt = parse_iso_timestamp(iso_str)
            if dt:
                # Convert UTC to local time for display
                local_dt = dt.astimezone()
                return local_dt.strftime(fmt)
        return iso_str
    except Exception:
        return iso_str[:5] if len(iso_str) >= 5 else iso_str


def format_age(seconds: float) -> str:
    """Format time ago in human readable form - pure function.
    Returns compact format: 5s, 3m, 2h, 1d (callers append ' ago' if needed).
    Returns "now" for 0 or negative (used for 'listening' status - heartbeat-proven current)."""
    if seconds <= 0:
        return "now"
    if seconds < 60:
        return f"{int(seconds)}s"
    elif seconds < 3600:
        return f"{int(seconds / 60)}m"
    elif seconds < 86400:
        return f"{int(seconds / 3600)}h"
    else:
        return f"{int(seconds / 86400)}d"


def format_listening_since(status_time: int | float) -> str:
    """Format 'since X' suffix for listening agents idle >= 1 minute.
    Returns ' since 5m' style suffix, empty string if idle < 1 minute."""
    import time

    if not status_time:
        return ""
    idle_secs = int(time.time()) - int(status_time)
    if idle_secs >= 60:
        return f" since {format_age(idle_secs)}"
    return ""


def get_status_counts(instances: dict[str, dict]) -> dict[str, int]:
    """Count instances by status type - pure data transformation"""
    counts = {s: 0 for s in STATUS_ORDER}
    for info in instances.values():
        status = info.get("status", "unknown")
        counts[status] = counts.get(status, 0) + 1
    return counts


# ===== Config Parsing Utilities =====
def parse_env_value(value: str) -> str:
    """Parse ENV file value with proper quote and escape handling"""
    value = value.strip()

    if not value:
        return value

    if value.startswith('"') and value.endswith('"') and len(value) >= 2:
        inner = value[1:-1]
        inner = inner.replace("\\\\", "\x00")
        inner = inner.replace("\\n", "\n")
        inner = inner.replace("\\t", "\t")
        inner = inner.replace("\\r", "\r")
        inner = inner.replace('\\"', '"')
        inner = inner.replace("\x00", "\\")
        return inner

    if value.startswith("'") and value.endswith("'") and len(value) >= 2:
        return value[1:-1]

    return value


def format_env_value(value: str) -> str:
    """Format value for ENV file with proper quoting (inverse of parse_env_value)"""
    if not value:
        return value

    # Check if quoting needed for special characters
    needs_quoting = any(c in value for c in ["\n", "\t", '"', "'", " ", "\r"])

    if needs_quoting:
        # Use double quotes with proper escaping
        escaped = value.replace("\\", "\\\\")  # Escape backslashes first
        escaped = escaped.replace("\n", "\\n")  # Escape newlines
        escaped = escaped.replace("\t", "\\t")  # Escape tabs
        escaped = escaped.replace("\r", "\\r")  # Escape carriage returns
        escaped = escaped.replace('"', '\\"')  # Escape double quotes
        return f'"{escaped}"'

    return value


def parse_env_file(config_path: Path) -> dict[str, str]:
    """Parse ENV file (KEY=VALUE format) with security validation"""
    config: dict[str, str] = {}

    dangerous_chars = ["`", "$", ";", "|", "&", "\n", "\r"]

    try:
        content = config_path.read_text(encoding="utf-8")
        for line in content.splitlines():
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            if "=" in line:
                key, _, value = line.partition("=")
                key = key.strip()
                value = value.strip()

                if key == "HCOM_TERMINAL":
                    # Check dangerous chars first (for all values)
                    if any(c in value for c in dangerous_chars):
                        print(
                            f"Warning: Unsafe characters in HCOM_TERMINAL "
                            f"({', '.join(repr(c) for c in dangerous_chars if c in value)}), "
                            f"ignoring custom terminal command",
                            file=sys.stderr,
                        )
                        continue
                    # Valid values: 'default', preset name, or custom command with {script}
                    # Internal/debug only: 'print' (show script), 'here' (force current terminal)
                    from .core.settings import get_merged_presets as _get_merged
                    _merged = _get_merged()
                    # Resolve old casing (WezTerm→wezterm, Alacritty→alacritty)
                    _lower_map = {k.lower(): k for k in _merged}
                    if value not in _merged and value.lower() in _lower_map:
                        value = _lower_map[value.lower()]
                    if (
                        value not in ("default", "print", "here")
                        and value not in _merged
                        and "{script}" not in value
                    ):
                        print(
                            "Warning: HCOM_TERMINAL must be 'default', a preset name, or custom command with {script}, "
                            "ignoring",
                            file=sys.stderr,
                        )
                        continue

                parsed = parse_env_value(value)
                if key:
                    config[key] = parsed
    except (FileNotFoundError, PermissionError, UnicodeDecodeError):
        pass
    return config


HCOM_SKIP_TOOL_ARGS_VALIDATION_ENV = "HCOM_SKIP_TOOL_ARGS_VALIDATION"


def skip_tool_args_validation() -> bool:
    """Return True if hcom should skip tool CLI arg validation.

    This bypasses hcom-side "unknown option"/parse error gating so the underlying
    tool (Claude/Gemini/Codex) can be the source of truth for flags.
    """
    value = os.environ.get(HCOM_SKIP_TOOL_ARGS_VALIDATION_ENV, "")
    return value not in ("", "0", "false", "False", "no", "NO")


__all__ = [
    # Version
    "__version__",
    # Platform detection
    "IS_WINDOWS",
    "CREATE_NO_WINDOW",
    "is_wsl",
    "is_termux",
    "termux_shebang_bypass",
    "TERMUX_NODE_PATH",
    "is_inside_ai_tool",
    "detect_vanilla_tool",
    # Tool arg validation override
    "HCOM_SKIP_TOOL_ARGS_VALIDATION_ENV",
    "skip_tool_args_validation",
    # Release configuration
    "RELEASED_TOOLS",
    "RELEASED_BACKGROUND",
    # Message constants
    "MENTION_PATTERN",
    "BIND_MARKER_RE",
    "SENDER",
    "SYSTEM_SENDER",
    "SENDER_EMOJI",
    "MAX_MESSAGES_PER_DELIVERY",
    "MAX_MESSAGE_SIZE",
    # Message identity
    "SenderIdentity",
    # Exceptions
    "HcomError",
    # Hook constants
    "STOP_HOOK_POLL_INTERVAL",
    # ANSI colors (re-exported from ui/colors.py)
    "RESET",
    "DIM",
    "BOLD",
    "REVERSE",
    "FG_GREEN",
    "FG_CYAN",
    "FG_WHITE",
    "FG_BLACK",
    "FG_GRAY",
    "FG_YELLOW",
    "FG_RED",
    "FG_BLUE",
    "FG_ORANGE",
    "FG_GOLD",
    "FG_LIGHTGRAY",
    "FG_DELIVER",
    "FG_STALE",
    "BG_BLUE",
    "BG_GREEN",
    "BG_CYAN",
    "BG_YELLOW",
    "BG_RED",
    "BG_GRAY",
    "BG_STALE",
    "BG_ORANGE",
    "BG_CHARCOAL",
    "BG_GOLD",
    "CLEAR_SCREEN",
    "CURSOR_HOME",
    "HIDE_CURSOR",
    "SHOW_CURSOR",
    "BOX_H",
    # Status name constants
    "ST_ACTIVE",
    "ST_LISTENING",
    "ST_BLOCKED",
    "ST_INACTIVE",
    "ST_LAUNCHING",
    "ST_ERROR",
    # Status configuration
    "STATUS_VALUES",
    "STATUS_ICONS",
    "ADHOC_ICON",
    "STATUS_COLORS",
    "STATUS_MAP",
    "STATUS_BG_COLORS",
    "STATUS_BG_MAP",
    "STATUS_ORDER",
    "STATUS_FG",
    # Config defaults
    # Terminal presets
    "TERMINAL_PRESETS",
    # Utility functions
    "shorten_path",
    "parse_iso_timestamp",
    "format_timestamp",
    "format_age",
    "get_status_counts",
    # Config parsing utilities
    "parse_env_value",
    "format_env_value",
    "parse_env_file",
]
