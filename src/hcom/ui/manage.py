"""Manage mode screen implementation.

This module implements the ManageScreen class, the primary view of the TUI.
It displays the instance list, recent messages, and message input area.

Screen Layout
-------------
    ┌─────────────────────────────────────────┐
    │ Instance List (scrollable)              │
    │   ▶ luna [listening] · 2m               │
    │   ○ nova [active] · now                 │
    │ ─────────────────────────────────────── │
    │ Instance Detail (when selected)         │
    │ ─────────────────────────────────────── │
    │ Messages (Slack-style)                  │
    │   10:23 luna                            │
    │         @nova ready when you are        │
    │ ─────────────────────────────────────── │
    │ > message input area                    │
    │ ─────────────────────────────────────── │
    └─────────────────────────────────────────┘

Key Bindings
------------
- UP/DOWN: Navigate instance list
- ENTER: Stop instance (two-step confirm) or send message if text entered
- @: Insert @mention for selected instance
- TAB: Switch to Launch mode
- Ctrl+K: Stop all instances (two-step confirm)
- Ctrl+R: Reset/archive session (two-step confirm)
- ESC: Clear input and close detail panel

Instance Display
----------------
Instances are shown with status icon, name, age, and description.
Tool type prefixes shown when multiple tools are present.
Color indicates status (green=active, cyan=listening, gray=stopped, etc).
"""

from __future__ import annotations
import re
import time
from typing import List, TYPE_CHECKING

if TYPE_CHECKING:
    from .tui import HcomTUI
    from .types import UIState

# Import rendering utilities
from .rendering import (
    ansi_len,
    ansi_ljust,
    bg_ljust,
    truncate_ansi,
    smart_truncate_name,
    get_terminal_size,
    AnsiTextWrapper,
    get_device_sync_color,
    separator_line,
    suppress_output,
)

# Import input utilities
from .input import (
    render_text_input,
    calculate_text_input_rows,
    text_input_insert,
    text_input_backspace,
    text_input_delete,
    text_input_move_left,
    text_input_move_right,
)

# Import ANSI codes directly to avoid circular import
from .colors import (
    RESET,
    BOLD,
    DIM,
    FG_WHITE,
    FG_GRAY,
    FG_YELLOW,
    FG_LIGHTGRAY,
    FG_ORANGE,
    FG_RED,
    FG_DELIVER,
    FG_CYAN,
    FG_BLACK,
    FG_STALE,
    BG_CHARCOAL,
    BG_GOLD,
)

# Import non-color constants from shared
from ..shared import (
    STATUS_FG,
    format_age,
    shorten_path,
    parse_iso_timestamp,
)
from ..core.instances import get_status_icon

# Import from source modules
from ..core.config import get_config
from ..commands.messaging import cmd_send
from ..commands.lifecycle import cmd_stop, cmd_fork


class ManageScreen:
    """Manage mode screen: instance list, messages, and input.

    This is the default/primary TUI screen. It displays:
    - List of active instances with status indicators
    - Recent message history with delivery status
    - Message composition input area

    The screen receives shared UIState and mutates it in response to
    user input. Rendering is done by build() which returns lines.

    Attributes:
        state: Reference to shared UIState object.
        tui: Reference to parent HcomTUI for flash notifications and commands.
    """

    def __init__(self, state: UIState, tui: HcomTUI):
        """Initialize ManageScreen.

        Args:
            state: Shared UIState object containing ManageState.
            tui: Parent HcomTUI for flash notifications and commands.
        """
        self.state = state  # Shared state (explicit dependency)
        self.tui = tui  # For commands only (flash, stop_all, etc)

    def _render_instance_row(
        self,
        name: str,
        info: dict,
        display_idx: int,
        name_col_width: int,
        width: int,
        is_remote: bool = False,
        show_tool: bool = False,
        project_tag: str = "",
        age_col_width: int = 10,
    ) -> str:
        """Render a single instance row with status indicator and details.

        Args:
            name: Instance display name (may include tag prefix).
            info: Instance info dict with status, data, age_text, etc.
            display_idx: Row index for cursor highlighting.
            name_col_width: Width allocated for name column.
            width: Total terminal width.
            is_remote: Whether this is a relay-synced remote instance.
            show_tool: Whether to show tool type prefix (claude/gemini/codex).
            project_tag: Project directory tag to show (if multiple dirs).

        Returns:
            Formatted line string with ANSI colors.
        """
        # Row exists = participating (no enabled field)
        status = info.get("status", "unknown")

        # Use get_status_icon for adhoc-aware icon selection
        instance_data = info.get("data", {})
        icon = get_status_icon(instance_data, status) if instance_data else "?"
        color = STATUS_FG.get(status, FG_WHITE)

        # Get binding status (needed for both tool prefix and timeout warning)
        from ..core.db import get_instance_bindings

        base_name = info.get("base_name", name)
        bindings = get_instance_bindings(base_name)

        # Tool prefix info (only when multiple tool types exist)
        tool_prefix_info = None
        if show_tool:
            tool = info.get("data", {}).get("tool", "claude")
            # Tool colors: foreground-only, dim
            tool_fg_colors = {
                "claude": "\033[38;5;173m",  # Muted rust (Anthropic)
                "gemini": "\033[38;5;140m",  # Muted purple (Google Gemini)
                "codex": "\033[38;5;73m",  # Muted teal (OpenAI/ChatGPT)
            }
            tool_fg = tool_fg_colors.get(tool, FG_GRAY)
            # Display based on binding: UPPER=pty+hooks, lower=hooks, UPPER*=pty only, lower*=none
            if bindings["process_bound"] and bindings["hooks_bound"]:
                tool_display = tool[:3]  # cla / gem / cod
            elif bindings["process_bound"]:
                tool_display = tool[:3] + "*"  # cla* - pty only (unusual)
            elif bindings["hooks_bound"]:
                tool_display = tool[:3]  # cla - hooks only
            elif tool != "adhoc":
                tool_display = tool[:3] + "*"  # cla* - no binding
            else:
                tool_display = "ah"  # ad-hoc tool type
            tool_label = tool_display.ljust(4)  # Pad to 4 chars
            tool_prefix_info = (tool_fg, tool_label)

        # Light green coloring for message delivery (active with deliver token)
        status_context = info.get("data", {}).get("status_context", "")
        if status == "active" and status_context.startswith("deliver:"):
            color = FG_DELIVER

        display_text = info.get("description", "")

        # Append "since X" for listening agents idle >= 1 minute
        if status == "listening" and display_text == "listening":
            from ..shared import format_listening_since

            status_time = info.get("data", {}).get("status_time", 0)
            display_text += format_listening_since(status_time)

        # Gold background for tui:* blocking status (gate blocked for 2+ seconds)
        if status == "listening" and status_context.startswith("tui:") and display_text:
            display_text = f"{BG_GOLD}{FG_BLACK} {display_text} {RESET}"

        age_text = info.get("age_text", "")
        # "now" special case (listening status uses age=0)
        age_str = age_text if age_text == "now" else (age_text if age_text else "")
        age_padded = age_str.rjust(age_col_width)

        # Badges
        is_background = info.get("data", {}).get("background", False)
        badges = ""
        if is_background:
            badges += " [headless]"
        # Project tag (shown when instances have different directories)
        project_suffix = ""
        if project_tag:
            project_suffix = f" {DIM}· {project_tag}"

        badge_visible_len = ansi_len(badges) + ansi_len(project_suffix)

        # Unread count - shown as left border indicator (count in detail view)
        unread_count = self.state.manage.unread_counts.get(name, 0)

        # Timeout warning for:
        # 1. Subagents (any tool) - always show, they have limited lifetime
        # 2. Claude hooks-only or headless - show for short timeouts (<1hr)
        timeout_marker = ""
        tool = info.get("data", {}).get("tool", "claude")
        data = info.get("data", {})
        age_seconds = info.get("age_seconds", 0)
        is_subagent = bool(data.get("parent_session_id"))

        # Subagents always show timeout warning (regardless of tool/binding)
        if status == "listening" and is_subagent:
            # Use parent's subagent_timeout if set, else global config
            parent_name = data.get("parent_name")
            timeout = None
            if parent_name:
                from ..core.instances import load_instance_position

                parent_data = load_instance_position(parent_name)
                if parent_data:
                    timeout = parent_data.get("subagent_timeout")
            if timeout is None:
                timeout = get_config().subagent_timeout
            remaining = timeout - age_seconds
            if 0 < remaining < 10:
                timeout_marker = f" {FG_YELLOW}⏱ {int(remaining)}s{RESET}"
        # Non-subagent Claude instances: show countdown for short timeouts (<1hr)
        elif status == "listening" and tool == "claude":
            is_hooks_only = bindings["hooks_bound"] and not bindings["process_bound"]
            is_headless = data.get("background", False)
            if is_hooks_only or is_headless:
                timeout = data.get("wait_timeout", get_config().timeout)
                if timeout < 3600:  # Only show countdown for <1hr timeouts
                    remaining = timeout - age_seconds
                    if 0 < remaining < 60:
                        timeout_marker = f" {FG_YELLOW}⏱ {int(remaining)}s{RESET}"

        max_name_len = name_col_width - badge_visible_len - 2
        display_name = smart_truncate_name(name, max_name_len)

        colored_name = display_name
        name_with_marker = f"{colored_name}{badges}{project_suffix}"
        name_padded = ansi_ljust(name_with_marker, name_col_width)

        desc_sep = ": " if display_text else ""
        weight = BOLD

        # Build tool prefix
        tool_prefix = ""
        if tool_prefix_info:
            tool_fg, tool_label = tool_prefix_info
            tool_prefix = f"{DIM}{tool_fg}{tool_label}{RESET}"

        # Left border indicators: orange for detail open, yellow for unread
        # Indicator adds 1 char width (pushes content right) - matches original detail behavior
        is_detail_open = self.state.manage.show_instance_detail == name
        has_unread = unread_count > 0
        if display_idx == self.state.manage.cursor:
            # Cursor row with charcoal background
            if is_detail_open:
                border = f"{FG_ORANGE}▐{RESET}{BG_CHARCOAL} "
            elif has_unread:
                border = f"{FG_YELLOW}▐{RESET}{BG_CHARCOAL} "
            else:
                border = f"{BG_CHARCOAL} "
            line = (
                f"{BG_CHARCOAL}{tool_prefix}{border}{color}{icon} {weight}{color}{name_padded}{RESET}{BG_CHARCOAL}"
                f"{FG_GRAY}{age_padded}{desc_sep}{display_text}{timeout_marker}{RESET}"
            )
            line = truncate_ansi(line, width)
            line = bg_ljust(line, width, BG_CHARCOAL)
        else:
            if has_unread:
                border = f"{FG_YELLOW}▐{RESET} "
            else:
                border = " "
            line = f"{tool_prefix}{border}{color}{icon}{RESET} {weight}{color}{name_padded}{RESET}{FG_GRAY}{age_padded}{desc_sep}{display_text}{timeout_marker}{RESET}"
            line = truncate_ansi(line, width)

        return line

    def _render_orphan_row(self, orphan: dict, display_idx: int, width: int) -> str:
        """Render an orphan process row (running hcom process not in active instances)"""
        pid = orphan["pid"]
        names = orphan.get("names", [])
        tool = orphan.get("tool", "unknown")
        launched_at = orphan.get("launched_at", 0)

        # Format: "  ◌ pid:12345 (luna, nova) · claude · 16m"
        names_str = f" ({', '.join(names)})" if names else ""
        age_str = format_age(time.time() - launched_at) if launched_at else ""
        content = f" ◌ pid:{pid}{names_str} · {tool}"
        if age_str:
            content += f" · {age_str}"

        # Check if kill confirmation is pending for this orphan
        orphan_key = f"orphan:{pid}"
        is_confirming = (
            self.state.confirm.pending_stop == orphan_key
            and (time.time() - self.state.confirm.pending_stop_time) <= self.tui.CONFIRMATION_TIMEOUT
        )

        if is_confirming:
            content += f"  {FG_RED}[Enter to kill]{RESET}"

        if display_idx == self.state.manage.cursor:
            line = f"{BG_CHARCOAL}{DIM}{FG_GRAY}{content}{RESET}"
            line = truncate_ansi(line, width)
            line = bg_ljust(line, width, BG_CHARCOAL)
        else:
            line = f"{DIM}{FG_GRAY}{content}{RESET}"
            line = truncate_ansi(line, width)

        return line

    def _render_recently_stopped_row(self, recently_stopped: list[str], display_idx: int, width: int) -> str:
        """Render the recently stopped summary row"""
        from ..core.db import RECENTLY_STOPPED_MINUTES

        # Build names list (truncate if too many)
        names = ", ".join(recently_stopped[:5])
        if len(recently_stopped) > 5:
            names += f" +{len(recently_stopped) - 5}"

        # Arrow indicates actionable (navigates to events)
        arrow = "[→]"

        # Format: "  ◌ Recently stopped (10m): luna, nova, kira  [→]"
        content = f" Recently stopped ({RECENTLY_STOPPED_MINUTES}m): {names}  {arrow}"

        # Styled distinctly from orphan PID rows (which use DIM FG_GRAY)
        if display_idx == self.state.manage.cursor:
            line = f"{BG_CHARCOAL}{DIM}{FG_STALE}{content}{RESET}"
            line = truncate_ansi(line, width)
            line = bg_ljust(line, width, BG_CHARCOAL)
        else:
            line = f"{DIM}{FG_STALE}{content}{RESET}"
            line = truncate_ansi(line, width)

        return line

    def build(self, height: int, width: int) -> List[str]:
        """Build the complete manage screen as a list of lines.

        Delegates to sub-methods for each section:
        - Instance list (with scrolling if needed)
        - Instance detail panel (if an instance is selected)
        - Message history
        - Message input area

        Args:
            height: Available height in terminal rows.
            width: Available width in terminal columns.

        Returns:
            List of formatted line strings ready for display.
        """
        layout_height = max(10, height)
        lines: List[str] = []
        instance_rows, message_rows, input_rows = self.calculate_layout(layout_height, width)

        # Instance list section
        self._build_instance_list(lines, instance_rows, width)

        # Separator between instances and messages/detail
        lines.append(separator_line(width))

        # Instance detail section (if active) - render ABOVE messages
        detail_rows = 0
        if self.state.manage.show_instance_detail:
            detail_lines = self.build_instance_detail(self.state.manage.show_instance_detail, width)
            lines.extend(detail_lines)
            detail_rows = len(detail_lines)
            lines.append(separator_line(width))
            detail_rows += 1  # Include separator in count

        # Messages section
        self._build_messages(lines, message_rows - detail_rows, width)

        # Pad and add input section at bottom
        input_section_height = input_rows + 2  # input + 2 separators
        max_lines_before_input = height - input_section_height

        if len(lines) > max_lines_before_input:
            lines = lines[:max_lines_before_input]
        while len(lines) < max_lines_before_input:
            lines.append("")

        lines.append(separator_line(width))
        lines.extend(self.render_wrapped_input(width, input_rows))
        lines.append(separator_line(width))

        return lines

    def _build_instance_list(self, lines: List[str], instance_rows: int, width: int):
        """Build instance list rows including cursor management and scrolling."""
        from ..core.instances import is_remote_instance
        from ..core.db import get_recently_stopped

        # Sort instances by creation time (newest first) - stable, no jumping
        all_instances = sorted(
            self.state.manage.instances.items(),
            key=lambda x: -x[1]["data"].get("created_at", 0.0),
        )

        # Separate local vs remote (row exists = participating, no stopped section)
        local_instances = [(n, i) for n, i in all_instances if not is_remote_instance(i.get("data", {}))]
        remote_instances = [(n, i) for n, i in all_instances if is_remote_instance(i.get("data", {}))]
        # Sort remote by created_at (all are participating)
        remote_instances.sort(key=lambda x: -x[1]["data"].get("created_at", 0.0))
        remote_count = len(remote_instances)

        # Get recently stopped instances (from events, last 10 min)
        active_names = set(self.state.manage.instances.keys())
        recently_stopped = get_recently_stopped(exclude_active=active_names)

        # Get orphan processes (running hcom-launched processes not in active instances)
        from ..core.pidtrack import get_orphan_processes

        active_pids = {i.get("data", {}).get("pid") for _, i in all_instances if i.get("data", {}).get("pid")}
        orphan_processes = get_orphan_processes(active_pids=active_pids)

        # Auto-expand remote section if user hasn't explicitly toggled
        # Expand if count <= 3 OR any device synced < 5min ago
        if not self.state.manage.show_remote_user_set and remote_count > 0:
            recent_sync = any(
                (time.time() - sync_time) < 300  # 5 minutes
                for sync_time in self.state.manage.device_sync_times.values()
                if sync_time
            )
            self.state.manage.show_remote = (remote_count <= 3) or recent_sync

        # Restore cursor position by instance name (stable across sorts)
        if self.state.manage.cursor_instance_name:
            found = False
            target_name = self.state.manage.cursor_instance_name
            last_cursor = self.state.manage.cursor

            # Check local instances
            for i, (name, _) in enumerate(local_instances):
                if name == target_name:
                    self.state.manage.cursor = i
                    found = True
                    break

            # Check remote instances (if not found and expanded)
            if not found and self.state.manage.show_remote:
                for i, (name, _) in enumerate(remote_instances):
                    if name == target_name:
                        # Position = local + orphans + stopped + separator + index
                        remote_sep_pos = len(local_instances) + len(orphan_processes) + (1 if recently_stopped else 0)
                        self.state.manage.cursor = remote_sep_pos + 1 + i
                        found = True
                        break

            if not found:
                # Instance disappeared (likely removed), move cursor to next logical position
                # Calculate total display count first
                temp_display_count = len(local_instances)
                temp_display_count += len(orphan_processes)
                if recently_stopped:
                    temp_display_count += 1  # recently stopped row
                if remote_count > 0:
                    temp_display_count += 1  # remote separator
                    if self.state.manage.show_remote:
                        temp_display_count += remote_count

                # Keep cursor at same position or move up if we were at the end
                if temp_display_count > 0:
                    self.state.manage.cursor = min(last_cursor, temp_display_count - 1)
                else:
                    self.state.manage.cursor = 0

                # Update cursor_instance_name to the instance now at cursor position
                # (Will be set below in the "Update tracked instance name" section)
                self.state.manage.cursor_instance_name = None
                self.sync_scroll_to_cursor()

        # Calculate total display items for cursor bounds
        # Order: local → orphans → recently_stopped → relay separator → remote
        display_count = len(local_instances)
        orphan_start_pos = display_count
        display_count += len(orphan_processes)
        if recently_stopped:
            display_count += 1  # recently stopped summary row
        recently_stopped_pos = display_count - 1 if recently_stopped else -1
        remote_sep = display_count if remote_count > 0 else -1
        if remote_count > 0:
            display_count += 1  # remote separator row
            if self.state.manage.show_remote:
                display_count += remote_count

        # Ensure cursor is valid
        if display_count > 0:
            self.state.manage.cursor = max(0, min(self.state.manage.cursor, display_count - 1))
            # Update tracked instance name (None if on separator/orphan/stopped)
            cursor = self.state.manage.cursor
            if cursor < len(local_instances):
                self.state.manage.cursor_instance_name = local_instances[cursor][0]
            elif remote_sep >= 0 and cursor == remote_sep:
                self.state.manage.cursor_instance_name = None  # Remote separator
            elif remote_sep >= 0 and self.state.manage.show_remote and cursor > remote_sep:
                remote_idx = cursor - remote_sep - 1
                if remote_idx < remote_count:
                    self.state.manage.cursor_instance_name = remote_instances[remote_idx][0]
                else:
                    self.state.manage.cursor_instance_name = None
            else:
                self.state.manage.cursor_instance_name = None
        else:
            self.state.manage.cursor = 0
            self.state.manage.cursor_instance_name = None

        # Empty state - no instances and no orphan processes
        if len(local_instances) == 0 and remote_count == 0 and not orphan_processes:
            lines.append("")
            lines.append(f"{FG_ORANGE}  ╦ ╦╔═╗╔═╗╔╦╗{RESET}")
            lines.append(f"{FG_ORANGE}  ╠═╣║  ║ ║║║║{RESET}")
            lines.append(f"{FG_ORANGE}  ╩ ╩╚═╝╚═╝╩ ╩{RESET}")
            lines.append("")
            lines.append(f"{FG_GRAY}  Realtime messaging for AI coding agents{RESET}")
            lines.append("")
            lines.append(f"{FG_WHITE}  Tab → LAUNCH{RESET}          {FG_GRAY}Start agents here{RESET}")
            lines.append(f"{FG_WHITE}  hcom 3 claude{RESET}         {FG_GRAY}Quick launch 3 Claudes{RESET}")
            lines.append(
                f"{FG_WHITE}  hcom start{RESET}            {FG_GRAY}Connect hcom from inside any session{RESET}"
            )
            lines.append("")
            lines.append(f"{FG_GRAY}  For all commands: hcom --help{RESET}")
            lines.append(f"{FG_GRAY}  For help: hcom claude 'help me! hcom!'{RESET}")

            lines.append("")
            # Pad to instance_rows
            while len(lines) < instance_rows:
                lines.append("")
            return

        # Calculate visible window
        max_scroll = max(0, display_count - instance_rows)
        self.state.manage.instance_scroll_pos = max(0, min(self.state.manage.instance_scroll_pos, max_scroll))

        # Calculate dynamic name column width based on actual names
        all_for_width = list(local_instances)
        if self.state.manage.show_remote:
            all_for_width += remote_instances
        max_instance_name_len = max((len(name) for name, _ in all_for_width), default=0)
        # Check if any instance has badges
        has_background = any(info.get("data", {}).get("background", False) for _, info in all_for_width)

        # Check if multiple tool types exist (show tool prefix if so)
        all_instances_for_tool = local_instances + remote_instances
        tool_types = set(info.get("data", {}).get("tool", "claude") for _, info in all_instances_for_tool)
        show_tool = len(tool_types) > 1

        # Check if multiple directories exist (show project tag if so)
        from ..shared import get_project_tag

        directories = set(
            info.get("data", {}).get("directory", "")
            for _, info in all_instances_for_tool
            if info.get("data", {}).get("directory")
        )
        show_project = len(directories) > 1

        # Calculate max badge length for column width
        badge_len = 0
        if has_background:
            badge_len += 11  # " [headless]"
        # Note: tool prefix (CLAUDE/CODEX) is rendered separately, not counted here
        if show_project:
            # " · " + max project tag length
            max_tag_len = max(
                (
                    len(get_project_tag(info.get("data", {}).get("directory", "")))
                    for _, info in all_for_width
                    if info.get("data", {}).get("directory")
                ),
                default=0,
            )
            badge_len += 3 + max_tag_len  # " · " + tag
        # Compute dynamic age column width from visible instances
        def _age_str_len(info: dict) -> int:
            age_text = info.get("age_text", "")
            s = age_text if age_text == "now" else (age_text if age_text else "")
            return len(s)

        age_col_width = max(
            (_age_str_len(info) for _, info in all_for_width),
            default=3,
        )
        age_col_width = max(3, age_col_width)  # minimum "now"

        # Add 2 for cursor icon/spacing
        name_col_width = max_instance_name_len + badge_len + 2
        # Only clamp name if it would leave no room for age+sep at all
        tool_prefix_width = 4 if show_tool else 0
        # Minimum reserved: tool + icon(2) + age + sep(2) — no description minimum
        min_reserved = 4 + age_col_width + tool_prefix_width
        name_col_width = min(name_col_width, max(6, width - min_reserved))

        # Build display rows
        visible_start = self.state.manage.instance_scroll_pos
        visible_end = min(visible_start + instance_rows, display_count)

        # If only 1 item would be hidden, show it instead of scroll indicator
        if visible_start == 1:
            visible_start = 0
        if display_count - visible_end == 1:
            visible_end = display_count

        for display_idx in range(visible_start, visible_end):
            # Order: local → orphans → recently_stopped → relay separator → remote
            if display_idx < len(local_instances):
                # Local instance
                name, info = local_instances[display_idx]
                project_tag = get_project_tag(info.get("data", {}).get("directory", "")) if show_project else ""
                line = self._render_instance_row(
                    name,
                    info,
                    display_idx,
                    name_col_width,
                    width,
                    show_tool=show_tool,
                    project_tag=project_tag,
                    age_col_width=age_col_width,
                )
                lines.append(line)
            elif orphan_start_pos <= display_idx < orphan_start_pos + len(orphan_processes):
                # Orphan process row
                orphan_idx = display_idx - orphan_start_pos
                line = self._render_orphan_row(orphan_processes[orphan_idx], display_idx, width)
                lines.append(line)
            elif recently_stopped_pos >= 0 and display_idx == recently_stopped_pos:
                # Recently stopped summary row
                line = self._render_recently_stopped_row(recently_stopped, display_idx, width)
                lines.append(line)
            elif remote_sep >= 0 and display_idx == remote_sep:
                # Relay separator row
                is_cursor = display_idx == self.state.manage.cursor
                arrow = "▼" if self.state.manage.show_remote else "▶"

                # Build sync status when expanded: relay (BOXE:1m, CATA:2s) ▼
                if self.state.manage.show_remote and self.state.manage.device_sync_times:
                    device_suffixes = {}
                    for name, info in remote_instances:
                        origin_device = info.get("data", {}).get("origin_device_id", "")
                        if origin_device and ":" in name:
                            suffix = name.rsplit(":", 1)[1]
                            device_suffixes[origin_device] = suffix

                    sync_parts = []
                    for device, sync_time in sorted(self.state.manage.device_sync_times.items()):
                        if sync_time:
                            sync_age = time.time() - sync_time
                            suffix = device_suffixes.get(device, device[:4].upper())
                            color = get_device_sync_color(sync_age)
                            sync_parts.append(f"{color}{suffix}:{format_age(sync_age)}{FG_GRAY}")

                    if sync_parts:
                        sep_text = f" relay ({', '.join(sync_parts)}) {arrow} "
                    else:
                        sep_text = f" relay ({remote_count}) {arrow} "
                else:
                    sep_text = f" relay ({remote_count}) {arrow} "

                text_len = ansi_len(sep_text)
                left_pad = max(0, (width - text_len) // 2)
                right_pad = max(0, width - text_len - left_pad)
                sep_line = f"{'─' * left_pad}{sep_text}{'─' * right_pad}"
                if is_cursor:
                    line = f"{BG_CHARCOAL}{FG_GRAY}{sep_line}{RESET}"
                    line = bg_ljust(line, width, BG_CHARCOAL)
                else:
                    line = f"{FG_GRAY}{sep_line}{RESET}"
                lines.append(truncate_ansi(line, width))
            elif remote_sep >= 0 and self.state.manage.show_remote and display_idx > remote_sep:
                # Remote instance (only when expanded)
                remote_idx = display_idx - remote_sep - 1
                if 0 <= remote_idx < remote_count:
                    name, info = remote_instances[remote_idx]
                    project_tag = get_project_tag(info.get("data", {}).get("directory", "")) if show_project else ""
                    line = self._render_instance_row(
                        name,
                        info,
                        display_idx,
                        name_col_width,
                        width,
                        is_remote=True,
                        show_tool=show_tool,
                        project_tag=project_tag,
                        age_col_width=age_col_width,
                    )
                    lines.append(line)

        # Add scroll indicators if needed
        if display_count > instance_rows:
            # If cursor will conflict with indicator, move cursor line first
            if visible_start > 0 and self.state.manage.cursor == visible_start:
                # Save cursor line (at position 0), move to position 1
                cursor_line = lines[0] if lines else ""
                lines[0] = lines[1] if len(lines) > 1 else ""
                if len(lines) > 1:
                    lines[1] = cursor_line

            if visible_end < display_count and self.state.manage.cursor == visible_end - 1:
                # Save cursor line (at position -1), move to position -2
                cursor_line = lines[-1] if lines else ""
                lines[-1] = lines[-2] if len(lines) > 1 else ""
                if len(lines) > 1:
                    lines[-2] = cursor_line

            # Now add indicators at edges (may overwrite moved content, that's fine)
            if visible_start > 0:
                count_above = visible_start
                indicator = f"{FG_GRAY}↑ {count_above} more{RESET}"
                if lines:
                    lines[0] = ansi_ljust(indicator, width)

            if visible_end < display_count:
                count_below = display_count - visible_end
                indicator = f"{FG_GRAY}↓ {count_below} more{RESET}"
                if lines:
                    lines[-1] = ansi_ljust(indicator, width)

        # Pad instances
        while len(lines) < instance_rows:
            lines.append("")

    def _build_messages(self, lines: List[str], message_rows: int, width: int):
        """Build message history section with read receipts and wrapping."""
        if self.state.manage.messages and message_rows > 0:
            all_wrapped_lines: List[str] = []

            # Get instance read positions for read receipt calculation
            # Keys are full display names to match delivered_to list
            instance_reads: dict = {}
            remote_instance_set: set = set()
            remote_msg_ts: dict = {}
            try:
                from ..core.db import get_db
                from ..core.instances import get_full_name

                conn = get_db()
                rows = conn.execute("SELECT name, last_event_id, origin_device_id, tag FROM instances").fetchall()
                # Track full_name -> base_name mapping for DB queries
                full_to_base = {}
                for row in rows:
                    full_name = get_full_name({"name": row["name"], "tag": row["tag"]}) or row["name"]
                    full_to_base[full_name] = row["name"]
                    instance_reads[full_name] = row["last_event_id"]
                    if row["origin_device_id"]:
                        remote_instance_set.add(full_name)
                # Get max msg_ts for remote instances from their status events
                for full_name in remote_instance_set:
                    base_name = full_to_base.get(full_name, full_name)
                    row = conn.execute(
                        """
                        SELECT json_extract(data, '$.msg_ts') as msg_ts
                        FROM events WHERE type = 'status' AND instance = ?
                          AND json_extract(data, '$.msg_ts') IS NOT NULL
                        ORDER BY id DESC LIMIT 1
                    """,
                        (base_name,),
                    ).fetchone()
                    if row and row["msg_ts"]:
                        remote_msg_ts[full_name] = row["msg_ts"]
            except Exception:
                pass  # No read receipts if DB query fails

            for (
                time_str,
                sender,
                message,
                delivered_to,
                event_id,
            ) in self.state.manage.messages:
                # Format timestamp (convert UTC to local time)
                dt = parse_iso_timestamp(time_str) if "T" in time_str else None
                display_time = (
                    dt.astimezone().strftime("%H:%M") if dt else (time_str[:5] if len(time_str) >= 5 else time_str)
                )

                # Build recipient list with read receipts (width-aware truncation)
                recipient_str = ""
                if delivered_to:
                    # Calculate available width for recipients
                    # Format: "HH:MM sender → recipients"
                    base_len = len(display_time) + 1 + len(sender) + 3  # +1 space, +3 for " → "
                    available = width - base_len - 5  # Reserve for "+N more"

                    recipient_parts = []
                    current_len = 0
                    shown = 0

                    for recipient in delivered_to:
                        # Check if recipient has read this message
                        if recipient in remote_instance_set:
                            has_read = remote_msg_ts.get(recipient, "") >= time_str
                        else:
                            has_read = instance_reads.get(recipient, 0) >= event_id
                        tick = " ✓" if has_read else ""
                        part = f"{recipient}{tick}"

                        # Calculate length with separator
                        part_len = ansi_len(part) + (2 if shown > 0 else 0)  # +2 for ", "

                        if current_len + part_len <= available:
                            recipient_parts.append(part)
                            current_len += part_len
                            shown += 1
                        else:
                            break

                    if recipient_parts:
                        recipient_str = ", ".join(recipient_parts)
                        remaining = len(delivered_to) - shown
                        if remaining > 0:
                            recipient_str += f" {FG_GRAY}+{remaining} more{RESET}"

                    if recipient_str:
                        recipient_str = f" {FG_GRAY}→{RESET} {recipient_str}"

                # Message recency: recent messages (< 30s) get brighter body text
                msg_age = (time.time() - dt.timestamp()) if dt else 9999.0
                is_recent = msg_age < 30.0
                body_color = FG_WHITE if is_recent else FG_LIGHTGRAY

                # Header line: timestamp + sender + recipients (truncated to width)
                header = f"{FG_GRAY}{display_time}{RESET} {BOLD}{sender}{RESET}{recipient_str}"
                header = truncate_ansi(header, width)
                all_wrapped_lines.append(header)

                # Replace literal newlines with space for preview
                display_message = message.replace("\n", " ")

                # Amber @mentions in message (e.g., @name or @name:DEVICE)
                if "@" in display_message:
                    display_message = re.sub(
                        r"(@[\w\-_:]+)",
                        f"{BOLD}{FG_ORANGE}\\1{RESET}{body_color}",
                        display_message,
                    )

                # Message lines with indent (4 spaces — tighter, more content visible)
                indent = "    "
                max_msg_len = width - len(indent)

                # Wrap message text
                if max_msg_len > 0:
                    wrapper = AnsiTextWrapper(width=max_msg_len)
                    wrapped = wrapper.wrap(display_message)

                    # All message lines indented uniformly
                    # Truncate to max_msg_len to prevent terminal wrapping on long unbreakable sequences
                    for wrapped_line in wrapped:
                        truncated = truncate_ansi(wrapped_line, max_msg_len)
                        line = f"{indent}{body_color}{truncated}{RESET}"
                        all_wrapped_lines.append(line)
                else:
                    # Fallback if width too small
                    all_wrapped_lines.append(f"{indent}{body_color}{display_message[: width - len(indent)]}{RESET}")

                # Blank line after each message (for separation)
                all_wrapped_lines.append("")

            # Take last N lines to fit available space (mid-message truncation)
            visible_lines = (
                all_wrapped_lines[-message_rows:]
                if len(all_wrapped_lines) > message_rows
                else all_wrapped_lines
            )
            lines.extend(visible_lines)
        else:
            # No messages - show hint only if instances exist (empty state shows logo instead)
            if self.state.manage.instances:
                lines.append(f"{FG_GRAY}No messages yet - type to compose | @ to mention{RESET}")

    def _get_display_lists(self):
        """Build local/remote instance lists for cursor navigation"""
        from ..core.instances import is_remote_instance
        from ..core.db import get_recently_stopped

        all_instances = sorted(
            self.state.manage.instances.items(),
            key=lambda x: -x[1]["data"].get("created_at", 0.0),
        )

        # Separate local vs remote (row exists = participating)
        local_instances = [(n, i) for n, i in all_instances if not is_remote_instance(i.get("data", {}))]
        remote_instances = [(n, i) for n, i in all_instances if is_remote_instance(i.get("data", {}))]
        # Sort remote by created_at (must match build())
        remote_instances.sort(key=lambda x: -x[1]["data"].get("created_at", 0.0))

        remote_count = len(remote_instances)

        # Get recently stopped names (excluding currently active)
        active_names = set(self.state.manage.instances.keys())
        recently_stopped = get_recently_stopped(exclude_active=active_names)

        # Get orphan processes (running but not in active instances)
        from ..core.pidtrack import get_orphan_processes

        active_pids = {i.get("data", {}).get("pid") for _, i in all_instances if i.get("data", {}).get("pid")}
        orphan_processes = get_orphan_processes(active_pids=active_pids)

        # Calculate display count: local → orphans → stopped → relay → remote
        display_count = len(local_instances)
        display_count += len(orphan_processes)
        if recently_stopped:
            display_count += 1  # recently stopped row
        if remote_count > 0:
            display_count += 1  # remote separator
            if self.state.manage.show_remote:
                display_count += remote_count

        return local_instances, remote_instances, display_count, recently_stopped, orphan_processes

    def _get_instance_at_cursor(self, local, remote, recently_stopped=None, orphan_processes=None):
        """Get (instance, is_remote, row_type) at cursor.

        Returns:
            (instance_tuple, is_remote, row_type) where row_type is:
            - 'instance': normal instance row
            - 'remote_sep': remote separator row
            - 'orphan': orphan process row (instance_tuple is the orphan dict)
            - 'recently_stopped': recently stopped summary row
            - None: unknown/empty
        """
        remote_count = len(remote)
        orphans = orphan_processes or []

        # Order: local → orphans → recently_stopped → relay separator → remote
        local_end = len(local)
        orphan_start = local_end
        orphan_end = orphan_start + len(orphans)

        recently_stopped_pos = orphan_end if recently_stopped else -1
        pos_after_stopped = orphan_end + (1 if recently_stopped else 0)

        remote_sep_pos = pos_after_stopped if remote_count > 0 else -1

        cursor = self.state.manage.cursor

        # Local section
        if cursor < local_end:
            return local[cursor], False, "instance"

        # Orphan process rows
        if orphans and orphan_start <= cursor < orphan_end:
            return orphans[cursor - orphan_start], False, "orphan"

        # Recently stopped row
        if recently_stopped_pos >= 0 and cursor == recently_stopped_pos:
            return None, False, "recently_stopped"

        # Remote separator
        if remote_count > 0 and cursor == remote_sep_pos:
            return None, False, "remote_sep"

        # Remote instances (if expanded)
        if remote_count > 0 and self.state.manage.show_remote:
            remote_start = remote_sep_pos + 1
            if remote_start <= cursor < remote_start + remote_count:
                return remote[cursor - remote_start], True, "instance"

        return None, False, None

    def _get_separator_positions(self, local, remote, recently_stopped=None, orphan_processes=None):
        """Calculate separator position for remote section"""
        remote_count = len(remote)
        orphans = orphan_processes or []
        pos = len(local) + len(orphans) + (1 if recently_stopped else 0)
        remote_sep = pos if remote_count > 0 else -1
        return remote_sep

    # Key handler methods for dispatch pattern
    def _handle_nav(self, key: str, local: list, remote: list, display_count: int, recently_stopped: list, orphan_processes: list | None = None):
        """Handle UP/DOWN navigation"""
        if key == "UP" and display_count > 0 and self.state.manage.cursor > 0:
            self.state.manage.cursor -= 1
        elif key == "DOWN" and display_count > 0 and self.state.manage.cursor < display_count - 1:
            self.state.manage.cursor += 1
        else:
            return

        inst, is_remote, row_type = self._get_instance_at_cursor(local, remote, recently_stopped, orphan_processes)
        self.state.manage.cursor_instance_name = inst[0] if (inst and row_type == "instance") else None
        self.tui.clear_all_pending_confirmations()
        self.state.manage.show_instance_detail = None
        # Cancel tag edit if navigating away
        if self.state.manage.tag_edit_target:
            self._handle_tag_cancel()
        self.sync_scroll_to_cursor()

    def _handle_at(self, local: list, remote: list, recently_stopped: list, orphan_processes: list | None = None):
        """Handle @ key - insert mention (disabled during tag edit)"""
        if self.state.manage.tag_edit_target:
            return  # Don't insert mentions into tag field
        self.tui.clear_all_pending_confirmations()
        inst, is_remote, row_type = self._get_instance_at_cursor(local, remote, recently_stopped, orphan_processes)
        if inst and row_type == "instance":
            name, _ = inst
            mention = f"@{name} "
            if mention not in self.state.manage.message_buffer:
                self.state.manage.message_buffer, self.state.manage.message_cursor_pos = text_input_insert(
                    self.state.manage.message_buffer, self.state.manage.message_cursor_pos, mention
                )

    def _handle_cursor_move(self, key: str):
        """Handle LEFT/RIGHT cursor movement"""
        self.tui.clear_all_pending_confirmations()
        if key == "LEFT":
            self.state.manage.message_cursor_pos = text_input_move_left(self.state.manage.message_cursor_pos)
        else:
            self.state.manage.message_cursor_pos = text_input_move_right(
                self.state.manage.message_buffer, self.state.manage.message_cursor_pos
            )

    def _handle_esc(self):
        """Handle ESC - cancel tag edit or clear everything"""
        if self.state.manage.tag_edit_target:
            return self._handle_tag_cancel()
        self.state.manage.message_buffer = ""
        self.state.manage.message_cursor_pos = 0
        self.state.manage.show_instance_detail = None
        self.tui.clear_all_pending_confirmations()

    def _handle_backspace(self):
        """Handle BACKSPACE - delete character"""
        self.tui.clear_all_pending_confirmations()
        self.state.manage.message_buffer, self.state.manage.message_cursor_pos = text_input_backspace(
            self.state.manage.message_buffer, self.state.manage.message_cursor_pos
        )

    def _handle_enter(self, local: list, remote: list, recently_stopped: list, orphan_processes: list | None = None):
        """Handle ENTER - send message, save tag, or toggle instance"""
        self.tui.clear_pending_confirmations_except("stop")

        # Tag edit mode: save tag
        if self.state.manage.tag_edit_target:
            return self._handle_tag_save()

        # Smart Enter: send message if text exists, otherwise toggle instances
        if self.state.manage.message_buffer.strip():
            return self._send_message()

        # Get what's at cursor
        inst, is_remote, row_type = self._get_instance_at_cursor(local, remote, recently_stopped, orphan_processes)

        # Handle special rows
        if row_type == "remote_sep":
            self.state.manage.show_remote = not self.state.manage.show_remote
            self.state.manage.show_remote_user_set = True
            return

        if row_type == "recently_stopped":
            return ("switch_events", {"view": "instances"})

        if row_type == "orphan":
            return self._handle_orphan_process(inst)

        if not inst:
            return

        name, info = inst
        if is_remote:
            return self._handle_remote_instance(name, info)
        return self._handle_local_instance(name, info)

    def _send_message(self):
        """Send message from buffer"""
        self.state.manage.send_state = "sending"
        self.state.frame_dirty = True
        self.tui.render()
        try:
            message = self.state.manage.message_buffer.strip()
            result = cmd_send(["--from", "bigboss", message])
            if result == 0:
                self.state.manage.send_state = "sent"
                self.state.manage.send_state_until = time.time() + 0.1
                self.state.manage.message_buffer = ""
                self.state.manage.message_cursor_pos = 0
            else:
                self.state.manage.send_state = None
                self.tui.flash_error("Send failed")
        except Exception as e:
            self.state.manage.send_state = None
            self.tui.flash_error(f"Error: {str(e)}")

    def _handle_remote_instance(self, name: str, info: dict):
        """Handle ENTER on remote instance - stop with confirmation"""
        from ..relay import send_control

        if ":" not in name:
            self.state.manage.show_instance_detail = name
            return

        base_name, device_short = name.rsplit(":", 1)
        status = info.get("status", "unknown")
        color = STATUS_FG.get(status, FG_WHITE)

        if (
            self.state.confirm.pending_stop == name
            and (time.time() - self.state.confirm.pending_stop_time) <= self.tui.CONFIRMATION_TIMEOUT
        ):
            if send_control("stop", base_name, device_short):
                self.tui.flash(f"Stopped hcom for {color}{name}{RESET}")
                self.tui.load_status()
            else:
                self.tui.flash_error("Failed to stop remote instance")
            self.state.confirm.pending_stop = None
            self.state.manage.show_instance_detail = None
        else:
            self.state.confirm.pending_stop = name
            self.state.confirm.pending_stop_time = time.time()
            self.state.manage.show_instance_detail = name

    def _handle_local_instance(self, name: str, info: dict):
        """Handle ENTER on local instance - stop with confirmation"""
        status = info.get("status", "unknown")
        color = STATUS_FG.get(status, FG_WHITE)

        status_context = info.get("data", {}).get("status_context", "")
        if status == "active" and status_context.startswith("deliver:"):
            color = FG_DELIVER

        if (
            self.state.confirm.pending_stop == name
            and (time.time() - self.state.confirm.pending_stop_time) <= self.tui.CONFIRMATION_TIMEOUT
        ):
            base_name = info.get("base_name", name)
            try:
                with suppress_output():
                    cmd_stop([base_name])
                self.tui.flash(f"Stopped hcom for {color}{name}{RESET}")
                self.tui.load_status()
            except Exception as e:
                self.tui.flash_error(f"Error: {str(e)}")
            finally:
                self.state.confirm.pending_stop = None
                self.state.manage.show_instance_detail = None
        else:
            self.state.confirm.pending_stop = name
            self.state.confirm.pending_stop_time = time.time()
            self.state.manage.show_instance_detail = name

    def _handle_orphan_process(self, orphan: dict):
        """Handle ENTER on orphan process row — kill with two-step confirmation"""
        pid = orphan["pid"]
        names = ", ".join(orphan.get("names", []))
        label = f"pid:{pid}" + (f" ({names})" if names else "")

        # Use pending_stop with a special prefix to avoid collision with instance stops
        orphan_key = f"orphan:{pid}"
        if (
            self.state.confirm.pending_stop == orphan_key
            and (time.time() - self.state.confirm.pending_stop_time) <= self.tui.CONFIRMATION_TIMEOUT
        ):
            # Second press — close terminal pane then kill
            from ..terminal import KillResult, kill_process
            result, _pane_closed = kill_process(pid, preset_name=orphan.get("terminal_preset", ""), pane_id=orphan.get("pane_id", ""), process_id=orphan.get("process_id", ""))
            if result == KillResult.PERMISSION_DENIED:
                self.tui.flash_error(f"Permission denied killing {label}")
            else:
                from ..core.pidtrack import remove_pid
                remove_pid(pid)
                if result == KillResult.ALREADY_DEAD:
                    self.tui.flash(f"{FG_GRAY}{label}{RESET} already dead")
                else:
                    self.tui.flash(f"Killed {FG_GRAY}{label}{RESET}")
            self.state.confirm.pending_stop = None
            self.state.manage.show_instance_detail = None
        else:
            # First press — show confirmation
            self.state.confirm.pending_stop = orphan_key
            self.state.confirm.pending_stop_time = time.time()
            self.tui.flash(
                f"{FG_WHITE}Kill {label}? (press Enter again){RESET}",
                duration=self.tui.CONFIRMATION_FLASH_DURATION,
                color="white",
            )

    def _handle_ctrl_k(self):
        """Handle CTRL_K - stop all instances with confirmation"""
        is_confirming = (
            self.state.confirm.pending_stop_all
            and (time.time() - self.state.confirm.pending_stop_all_time) <= self.tui.CONFIRMATION_TIMEOUT
        )
        self.tui.clear_pending_confirmations_except("stop_all")

        if is_confirming:
            self.tui.stop_all_instances()
            self.state.confirm.pending_stop_all = False
        else:
            self.state.confirm.pending_stop_all = True
            self.state.confirm.pending_stop_all_time = time.time()
            self.tui.flash(
                f"{FG_WHITE}Confirm stop all instances? (press Ctrl+K again){RESET}",
                duration=self.tui.CONFIRMATION_FLASH_DURATION,
                color="white",
            )

    def _handle_ctrl_r(self):
        """Handle CTRL_R - reset with confirmation"""
        is_confirming = (
            self.state.confirm.pending_reset
            and (time.time() - self.state.confirm.pending_reset_time) <= self.tui.CONFIRMATION_TIMEOUT
        )
        self.tui.clear_pending_confirmations_except("reset")

        if is_confirming:
            self.tui.reset_events()
            self.state.confirm.pending_reset = False
        else:
            self.state.confirm.pending_reset = True
            self.state.confirm.pending_reset_time = time.time()
            self.tui.flash(
                f"{FG_WHITE}Confirm clear & archive (conversation + instance list)? (press Ctrl+R again){RESET}",
                duration=self.tui.CONFIRMATION_FLASH_DURATION,
                color="white",
            )

    def _handle_fork(self, local: list, remote: list, recently_stopped: list, orphan_processes: list | None = None):
        """Handle f key - fork instance (immediate, no confirmation)"""
        inst, is_remote, row_type = self._get_instance_at_cursor(local, remote, recently_stopped, orphan_processes)

        if not inst or row_type != "instance" or is_remote:
            return

        name, info = inst
        base_name = info.get("base_name", name)

        try:
            with suppress_output():
                result = cmd_fork([base_name])
            if result == 0:
                self.tui.flash(f"Forked {name}")
                self.tui.load_status()
            else:
                self.tui.flash_error("Fork failed")
        except Exception as e:
            self.tui.flash_error(f"Fork error: {str(e)}")

    def _handle_tag_start(self, local: list, remote: list, recently_stopped: list, orphan_processes: list | None = None):
        """Handle t key - start tag editing mode"""
        inst, is_remote, row_type = self._get_instance_at_cursor(local, remote, recently_stopped, orphan_processes)

        if not inst or row_type != "instance" or is_remote:
            return

        name, info = inst
        base_name = info.get("base_name", name)
        current_tag = info.get("data", {}).get("tag") or ""

        # Enter tag edit mode (save original buffer and cursor)
        self.state.manage.tag_edit_target = base_name
        self.state.manage.tag_edit_original_buffer = self.state.manage.message_buffer
        self.state.manage.tag_edit_original_cursor = self.state.manage.message_cursor_pos
        self.state.manage.message_buffer = current_tag
        self.state.manage.message_cursor_pos = len(current_tag)
        self.state.manage.show_instance_detail = name
        self.tui.clear_all_pending_confirmations()

    def _handle_tag_save(self):
        """Save tag and exit tag edit mode"""
        from ..core.db import update_instance
        import re

        target = self.state.manage.tag_edit_target
        if not target:
            return

        new_tag = self.state.manage.message_buffer.strip()

        # Validate: alphanumeric + dash/underscore, or empty
        if new_tag and not re.match(r"^[a-zA-Z0-9_-]+$", new_tag):
            self.tui.flash_error("Tag must be alphanumeric (a-z, 0-9, -, _)")
            return

        # Save to DB (empty string becomes None)
        try:
            if not update_instance(target, {"tag": new_tag if new_tag else None}):
                self.tui.flash_error("Failed to save tag")
                return
        except Exception as e:
            self.tui.flash_error(f"Tag save error: {e}")
            return

        # Exit tag edit mode
        self.state.manage.tag_edit_target = None
        self.state.manage.message_buffer = ""
        self.state.manage.message_cursor_pos = 0
        self.state.manage.tag_edit_original_buffer = ""
        self.state.manage.show_instance_detail = None

        self.tui.load_status()
        self.tui.flash(f"Tag {'set to ' + new_tag if new_tag else 'removed'}")

        # Wake PTY delivery thread so terminal title updates instantly
        from ..core.runtime import notify_instance
        notify_instance(target)

    def _handle_tag_cancel(self):
        """Cancel tag editing and restore original buffer and cursor"""
        self.state.manage.message_buffer = self.state.manage.tag_edit_original_buffer
        self.state.manage.message_cursor_pos = self.state.manage.tag_edit_original_cursor
        self.state.manage.tag_edit_target = None
        self.state.manage.tag_edit_original_buffer = ""
        self.state.manage.tag_edit_original_cursor = 0
        self.state.manage.show_instance_detail = None

    def _handle_text_input(self, key: str):
        """Handle text input - space, newline, printable chars"""
        self.tui.clear_all_pending_confirmations()
        char = " " if key == "SPACE" else ("\n" if key == "\n" else key)
        self.state.manage.message_buffer, self.state.manage.message_cursor_pos = text_input_insert(
            self.state.manage.message_buffer, self.state.manage.message_cursor_pos, char
        )

    def handle_key(self, key: str):
        """Handle keyboard input in Manage mode.

        Uses a dispatch pattern to route keys to appropriate handlers.
        Updates state and may return commands for the TUI orchestrator.

        Args:
            key: Key name from KeyboardInput (e.g., "UP", "ENTER", "a").

        Returns:
            None for most keys, or a tuple like ("switch_events", {...})
            to signal mode changes to the orchestrator.
        """
        local, remote, display_count, recently_stopped, orphan_processes = self._get_display_lists()
        self._get_separator_positions(local, remote)

        # Dispatch table for simple key handlers
        if key in ("UP", "DOWN"):
            return self._handle_nav(key, local, remote, display_count, recently_stopped, orphan_processes)
        elif key == "@":
            return self._handle_at(local, remote, recently_stopped, orphan_processes)
        elif key in ("LEFT", "RIGHT"):
            return self._handle_cursor_move(key)
        elif key == "ESC":
            return self._handle_esc()
        elif key == "BACKSPACE":
            return self._handle_backspace()
        elif key == "DELETE":
            self.state.manage.message_buffer, self.state.manage.message_cursor_pos = text_input_delete(
                self.state.manage.message_buffer, self.state.manage.message_cursor_pos
            )
        elif key in ("HOME", "CTRL_A"):
            self.state.manage.message_cursor_pos = 0
        elif key in ("END", "CTRL_E"):
            self.state.manage.message_cursor_pos = len(self.state.manage.message_buffer)
        elif key == "ENTER":
            return self._handle_enter(local, remote, recently_stopped, orphan_processes)
        elif key == "CTRL_K":
            return self._handle_ctrl_k()
        elif key == "CTRL_R":
            return self._handle_ctrl_r()
        # Fork and tag: Ctrl+F and Ctrl+T (avoids collision with typing)
        elif key == "CTRL_F" and not self.state.manage.tag_edit_target:
            return self._handle_fork(local, remote, recently_stopped, orphan_processes)
        elif key == "CTRL_T" and not self.state.manage.tag_edit_target:
            return self._handle_tag_start(local, remote, recently_stopped, orphan_processes)
        elif key in ("SPACE", "\n") or (key and len(key) == 1 and key.isprintable()):
            return self._handle_text_input(key)

    def calculate_layout(self, height: int, width: int) -> tuple[int, int, int]:
        """Calculate instance/message/input row allocation"""
        from ..core.instances import is_remote_instance
        from ..core.db import get_recently_stopped

        # Dynamic input area based on buffer size
        input_rows = calculate_text_input_rows(self.state.manage.message_buffer, width)
        # Space budget
        separator_rows = 3  # One separator between instances and messages, one before input, one after input
        min_instance_rows = 3

        available = height - input_rows - separator_rows

        # Calculate display count based on current collapse state
        all_instances = list(self.state.manage.instances.values())
        local_instances = [i for i in all_instances if not is_remote_instance(i.get("data", {}))]
        remote_instances = [i for i in all_instances if is_remote_instance(i.get("data", {}))]

        local_count = len(local_instances)
        remote_count = len(remote_instances)

        # Get recently stopped for display count
        active_names = set(self.state.manage.instances.keys())
        recently_stopped = get_recently_stopped(exclude_active=active_names)

        # Get orphan process count
        from ..core.pidtrack import get_orphan_processes

        active_pids = {i.get("data", {}).get("pid") for i in all_instances if i.get("data", {}).get("pid")}
        orphan_count = len(get_orphan_processes(active_pids=active_pids))

        # Build display count: local + remote section + orphans + recently_stopped
        display_count = local_count
        if remote_count > 0:
            display_count += 1  # remote separator
            if self.state.manage.show_remote:
                display_count += remote_count
        display_count += orphan_count
        if recently_stopped:
            display_count += 1  # recently stopped row

        max_instance_rows = int(available * 0.6)
        instance_rows = max(min_instance_rows, min(display_count, max_instance_rows))
        message_rows = available - instance_rows

        return instance_rows, message_rows, input_rows

    def sync_scroll_to_cursor(self):
        """Sync scroll position to cursor"""
        # Calculate visible rows using shared layout function
        width, rows = get_terminal_size()
        body_height = max(10, rows - 3)  # Header, flash, footer
        instance_rows, _, _ = self.calculate_layout(body_height, width)
        visible_instance_rows = instance_rows  # Full instance section is visible

        # Scroll up if cursor moved above visible window
        if self.state.manage.cursor < self.state.manage.instance_scroll_pos:
            self.state.manage.instance_scroll_pos = self.state.manage.cursor
        # Scroll down if cursor moved below visible window
        elif self.state.manage.cursor >= self.state.manage.instance_scroll_pos + visible_instance_rows:
            self.state.manage.instance_scroll_pos = self.state.manage.cursor - visible_instance_rows + 1

    def render_wrapped_input(self, width: int, input_rows: int) -> List[str]:
        """Render message input (delegates to shared helper)"""
        prefix = "Tag: " if self.state.manage.tag_edit_target else "> "
        return render_text_input(
            self.state.manage.message_buffer,
            self.state.manage.message_cursor_pos,
            width,
            input_rows,
            prefix=prefix,
            send_state=self.state.manage.send_state,
        )

    def build_instance_detail(self, name: str, width: int) -> List[str]:
        """Build instance metadata display (similar to hcom list --verbose)"""
        import time
        from ..core.instances import is_remote_instance

        lines = []

        # Get instance data
        if name not in self.state.manage.instances:
            return [f"{FG_GRAY}Instance not found{RESET}"]

        info = self.state.manage.instances[name]
        data = info["data"]

        # Get status color for name (same as flash message)
        status = info.get("status", "unknown")
        color = STATUS_FG.get(status, FG_WHITE)

        # Light green coloring for message delivery (active with deliver token)
        status_context = data.get("status_context", "")
        if status == "active" and status_context.startswith("deliver:"):
            color = FG_DELIVER

        # Header: bold colored name (badges already shown in instance list)
        header = f"{BOLD}{color}{name}{RESET}"
        lines.append(truncate_ansi(header, width))

        # Unread message count (aligned with other fields at column 16)
        unread_count = self.state.manage.unread_counts.get(name, 0)
        if unread_count > 0:
            lines.append(
                truncate_ansi(
                    f"  {FG_YELLOW}unread:       {unread_count} message{'s' if unread_count != 1 else ''}{RESET}",
                    width,
                )
            )

        if is_remote_instance(data):
            # Remote instance: show device/sync info plus available details
            origin_device = data.get("origin_device_id", "")
            device_short = origin_device[:8] if origin_device else "(unknown)"

            # Get device sync time
            sync_time = self.state.manage.device_sync_times.get(origin_device, 0)
            sync_str = f"{format_age(time.time() - sync_time)} ago" if sync_time else "never"

            lines.append(truncate_ansi(f"  device:      {device_short}", width))
            lines.append(truncate_ansi(f"  last_sync:   {sync_str}", width))

            # Show available remote instance details
            session_id = data.get("session_id") or "(none)"
            tool = data.get("tool", "claude")
            lines.append(truncate_ansi(f"  session_id:  {session_id}", width))
            lines.append(truncate_ansi(f"  tool:        {tool}", width))

            parent = data.get("parent_name")
            if parent:
                lines.append(truncate_ansi(f"  parent:      {parent}", width))

            directory = data.get("directory")
            if directory:
                lines.append(truncate_ansi(f"  directory:   {shorten_path(directory)}", width))

            # Format status_time
            status_time = data.get("status_time", 0)
            if status_time:
                lines.append(
                    truncate_ansi(
                        f"  status_at:   {format_age(time.time() - status_time)} ago",
                        width,
                    )
                )

            last_stop = data.get("last_stop", 0)
            if last_stop:
                lines.append(
                    truncate_ansi(
                        f"  last_stop:   {format_age(time.time() - last_stop)} ago",
                        width,
                    )
                )
        else:
            # Local instance: show full details
            session_id = data.get("session_id") or "None"
            directory = data.get("directory") or "(none)"
            parent = data.get("parent_name") or None

            # Format paths (shorten with ~)
            directory = shorten_path(directory) if directory != "(none)" else directory
            log_file = shorten_path(data.get("background_log_file"))
            transcript = shorten_path(data.get("transcript_path")) or "(none)"

            # Format created_at timestamp
            created_ts = data.get("created_at")
            created = f"{format_age(time.time() - created_ts)} ago" if created_ts else "(unknown)"

            # Tool type
            tool = data.get("tool", "claude")

            # Build detail lines (truncated to terminal width)
            lines.append(truncate_ansi(f"  session_id:  {session_id}", width))
            lines.append(truncate_ansi(f"  tool:        {tool}", width))

            # Show status_detail if present
            status_detail = data.get("status_detail", "")
            if status_detail:
                lines.append(truncate_ansi(f"  detail:      {status_detail}", width))

            lines.append(truncate_ansi(f"  created:     {created}", width))
            lines.append(truncate_ansi(f"  directory:   {directory}", width))

            if parent:
                agent_id = data.get("agent_id") or "(none)"
                lines.append(truncate_ansi(f"  parent:      {parent}", width))
                lines.append(truncate_ansi(f"  agent_id:    {agent_id}", width))

            # Show binding status (integration tier): pty/hooks/none
            from ..core.db import get_instance_bindings, format_binding_status

            base_name = info.get("base_name", name)
            bindings = get_instance_bindings(base_name)
            bind_str = format_binding_status(bindings)
            lines.append(truncate_ansi(f"  bindings:    {bind_str}", width))

            # Terminal preset (tmux/kitty/wezterm/etc)
            lc_str = data.get("launch_context", "")
            if lc_str:
                try:
                    import json as _json
                    lc = _json.loads(lc_str) if isinstance(lc_str, str) else lc_str
                    preset = lc.get("terminal_preset", "")
                    if preset:
                        lines.append(truncate_ansi(f"  terminal:    {preset}", width))
                except (ValueError, TypeError):
                    pass

            if log_file:
                lines.append(truncate_ansi(f"  log:         {log_file}", width))

            lines.append(truncate_ansi(f"  transcript:  {transcript}", width))

        # Show available actions
        lines.append("")
        if self.state.manage.tag_edit_target:
            # Tag edit mode
            lines.append(truncate_ansi(f"{FG_CYAN}[Enter]{RESET} Save tag  {FG_CYAN}[Esc]{RESET} Cancel", width))
        elif is_remote_instance(data):
            # Remote: stop only
            lines.append(truncate_ansi(f"{FG_CYAN}[Enter]{RESET} Stop hcom", width))
        else:
            # Local: stop, fork, tag
            lines.append(truncate_ansi(f"{FG_CYAN}[Enter]{RESET} Stop  {FG_CYAN}[^F]{RESET} Fork  {FG_CYAN}[^T]{RESET} Tag", width))

        return lines
