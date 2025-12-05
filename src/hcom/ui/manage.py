"""Manage mode screen implementation"""
from __future__ import annotations
import re
import time
from typing import List, TYPE_CHECKING

if TYPE_CHECKING:
    from .tui import HcomTUI
    from .types import UIState

# Import rendering utilities
from .rendering import (
    ansi_len, ansi_ljust, bg_ljust, truncate_ansi,
    smart_truncate_name, get_terminal_size, AnsiTextWrapper,
    get_device_sync_color, separator_line,
)

# Import input utilities
from .input import (
    render_text_input, calculate_text_input_rows,
    text_input_insert, text_input_backspace,
    text_input_move_left, text_input_move_right
)

# Import from shared
from ..shared import (
    RESET, BOLD, DIM,
    FG_WHITE, FG_GRAY, FG_YELLOW, FG_LIGHTGRAY, FG_ORANGE, FG_GOLD, FG_DELIVER,
    FG_GREEN, FG_RED,
    BG_CHARCOAL, BG_GRAY,
    STATUS_MAP, STATUS_FG,
    format_age, shorten_path, parse_iso_timestamp,
)

# Import from api
from ..api import (
    get_config,
    cmd_send, cmd_start, cmd_stop,
)

import sys
from contextlib import contextmanager
from io import StringIO

@contextmanager
def _suppress_output():
    """Capture stdout/stderr to prevent CLI output from corrupting TUI display"""
    old_stdout, old_stderr = sys.stdout, sys.stderr
    sys.stdout, sys.stderr = StringIO(), StringIO()
    try:
        yield
    finally:
        sys.stdout, sys.stderr = old_stdout, old_stderr


class ManageScreen:
    """Manage mode: instance list + messages + input"""

    def __init__(self, state: UIState, tui: HcomTUI):
        self.state = state  # Shared state (explicit dependency)
        self.tui = tui      # For commands only (flash, stop_all, etc)

    def _render_instance_row(self, name: str, info: dict, display_idx: int, name_col_width: int, width: int, is_remote: bool = False) -> str:
        """Render a single instance row"""
        from ..core.instances import is_external_sender, is_remote_instance

        enabled = info.get('enabled', False)
        status = info.get('status', "unknown")
        _, icon = STATUS_MAP.get(status, (BG_GRAY, '?'))
        color = STATUS_FG.get(status, FG_WHITE)

        # Light green coloring for message delivery (active with deliver token)
        status_context = info.get('data', {}).get('status_context', '')
        if status == 'active' and status_context.startswith('deliver:'):
            color = FG_DELIVER

        display_text = info.get('description', '')
        age_text = info.get('age_text', '')
        age_str = f"{age_text} ago" if age_text else ""
        age_padded = age_str.rjust(10)

        # Badges
        is_background = info.get('data', {}).get('background', False)
        is_ext = is_external_sender(info.get('data', {}))
        badges = ""
        if is_background:
            badges += " [headless]"
        if is_ext:
            badges += " [external]"
        badge_visible_len = len(badges)

        # Timeout warning
        timeout_marker = ""
        if enabled and status == "idle":
            age_seconds = info.get('age_seconds', 0)
            data = info.get('data', {})
            is_subagent = bool(data.get('parent_session_id'))
            if is_subagent:
                timeout = get_config().subagent_timeout
                remaining = timeout - age_seconds
                if 0 < remaining < 10:
                    timeout_marker = f" {FG_YELLOW}⏱ {int(remaining)}s{RESET}"
            else:
                timeout = data.get('wait_timeout', get_config().timeout)
                remaining = timeout - age_seconds
                if 0 < remaining < 60:
                    timeout_marker = f" {FG_YELLOW}⏱ {int(remaining)}s{RESET}"

        max_name_len = name_col_width - badge_visible_len - 2
        display_name = smart_truncate_name(name, max_name_len)

        colored_name = display_name

        # State indicator (on cursor row)
        if display_idx == self.state.cursor:
            is_pending = self.state.pending_toggle == name and (time.time() - self.state.pending_toggle_time) <= self.tui.CONFIRMATION_TIMEOUT
            if is_pending:
                state_symbol = "±"
            elif enabled:
                state_symbol = "+"
            else:
                state_symbol = "-"
            name_with_marker = f"{colored_name}{badges} {state_symbol}"
            name_padded = ansi_ljust(name_with_marker, name_col_width)
        else:
            name_with_marker = f"{colored_name}{badges}"
            name_padded = ansi_ljust(name_with_marker, name_col_width)

        desc_sep = ": " if display_text else ""
        weight = BOLD if enabled else DIM

        if display_idx == self.state.cursor:
            line = f"{BG_CHARCOAL}{color}{icon} {weight}{color}{name_padded}{RESET}{BG_CHARCOAL}{weight}{FG_GRAY}{age_padded}{desc_sep}{display_text}{timeout_marker}{RESET}"
            line = truncate_ansi(line, width)
            line = bg_ljust(line, width, BG_CHARCOAL)
        else:
            line = f"{color}{icon}{RESET} {weight}{color}{name_padded}{RESET}{weight}{FG_GRAY}{age_padded}{desc_sep}{display_text}{timeout_marker}{RESET}"
            line = truncate_ansi(line, width)

        return line

    def build(self, height: int, width: int) -> List[str]:
        """Build manage screen layout"""
        # Use minimum height for layout calculation to maintain structure
        layout_height = max(10, height)

        lines = []

        # Calculate layout using shared function
        instance_rows, message_rows, input_rows = self.calculate_layout(layout_height, width)

        # Launch status banner (if active batch not yet ready)
        if self.state.launch_batch:
            batch = self.state.launch_batch
            if self.state.launch_batch_failed and time.time() < self.state.launch_batch_failed_until:
                # Failed state - red banner for 5s
                banner = f"{FG_RED}Launch failed: {batch['ready']}/{batch['expected']} ready (timed out){RESET}"
            else:
                # Normal launching state - yellow with dots
                dots = '.' * (1 + int(time.time()) % 3)
                banner = f"{FG_YELLOW}Launching{dots} {batch['ready']}/{batch['expected']} ready{RESET}"
            lines.append(banner)
            instance_rows -= 1  # Reserve row for banner

        from ..core.instances import is_remote_instance

        # Sort instances by creation time (newest first) - stable, no jumping
        all_instances = sorted(
            self.state.instances.items(),
            key=lambda x: -x[1]['data'].get('created_at', 0.0)
        )

        # Separate local vs remote, then enabled vs stopped (remote handled separately)
        local_instances = [(n, i) for n, i in all_instances if not is_remote_instance(i.get('data', {}))]
        remote_instances = [(n, i) for n, i in all_instances if is_remote_instance(i.get('data', {}))]
        # Sort remote: enabled first, then by created_at
        remote_instances.sort(key=lambda x: (not x[1].get('enabled', False), -x[1]['data'].get('created_at', 0.0)))
        remote_count = len(remote_instances)

        # Local instances: enabled and stopped
        enabled_instances = [(n, i) for n, i in local_instances if i.get('enabled', False)]
        stopped_instances = [(n, i) for n, i in local_instances if not i.get('enabled', False)]
        stopped_count = len(stopped_instances)

        # Auto-expand sections if user hasn't explicitly toggled
        # Expand if count <= 3 OR (for remote) any device synced < 5min ago
        if not self.state.show_stopped_user_set and stopped_count > 0:
            self.state.show_stopped = stopped_count <= 3

        if not self.state.show_remote_user_set and remote_count > 0:
            recent_sync = any(
                (time.time() - sync_time) < 300  # 5 minutes
                for sync_time in self.state.device_sync_times.values()
                if sync_time
            )
            has_enabled_remote = any(i.get('enabled', False) for _, i in remote_instances)
            self.state.show_remote = (remote_count <= 3 and has_enabled_remote) or recent_sync

        # Restore cursor position by instance name (stable across sorts)
        # Must account for separator row offsets when matching stopped/remote instances
        if self.state.cursor_instance_name:
            found = False
            target_name = self.state.cursor_instance_name

            # Check enabled instances
            for i, (name, _) in enumerate(enabled_instances):
                if name == target_name:
                    self.state.cursor = i
                    found = True
                    break

            # Check stopped instances (if not found and expanded)
            if not found and self.state.show_stopped:
                for i, (name, _) in enumerate(stopped_instances):
                    if name == target_name:
                        # Position = enabled_count + 1 (separator) + index
                        self.state.cursor = len(enabled_instances) + 1 + i
                        found = True
                        break

            # Check remote instances (if not found and expanded)
            if not found and self.state.show_remote:
                for i, (name, _) in enumerate(remote_instances):
                    if name == target_name:
                        # Position = enabled + stopped_section + 1 (remote separator) + index
                        stopped_section = 0
                        if stopped_count > 0:
                            stopped_section = 1 + (stopped_count if self.state.show_stopped else 0)
                        self.state.cursor = len(enabled_instances) + stopped_section + 1 + i
                        found = True
                        break

            if not found:
                # Instance disappeared, reset cursor
                self.state.cursor = 0
                self.state.cursor_instance_name = None
                self.sync_scroll_to_cursor()

        # Calculate total display items for cursor bounds (enabled + stopped + remote)
        display_count = len(enabled_instances)
        if stopped_count > 0:
            display_count += 1  # stopped separator row
            if self.state.show_stopped:
                display_count += stopped_count
        if remote_count > 0:
            display_count += 1  # remote separator row
            if self.state.show_remote:
                display_count += remote_count

        # Calculate separator positions for cursor tracking
        stopped_sep = len(enabled_instances) if stopped_count > 0 else -1
        if remote_count > 0:
            if stopped_count > 0:
                remote_sep = stopped_sep + 1 + (stopped_count if self.state.show_stopped else 0)
            else:
                remote_sep = len(enabled_instances)
        else:
            remote_sep = -1

        # Ensure cursor is valid
        if display_count > 0:
            self.state.cursor = max(0, min(self.state.cursor, display_count - 1))
            # Update tracked instance name (None if on separator)
            cursor = self.state.cursor
            if cursor < len(enabled_instances):
                self.state.cursor_instance_name = enabled_instances[cursor][0]
            elif stopped_sep >= 0 and cursor == stopped_sep:
                self.state.cursor_instance_name = None  # Stopped separator
            elif self.state.show_stopped and stopped_count > 0 and cursor < stopped_sep + 1 + stopped_count:
                stopped_idx = cursor - stopped_sep - 1
                if stopped_idx < stopped_count:
                    self.state.cursor_instance_name = stopped_instances[stopped_idx][0]
                else:
                    self.state.cursor_instance_name = None
            elif remote_sep >= 0 and cursor == remote_sep:
                self.state.cursor_instance_name = None  # Remote separator
            elif self.state.show_remote and remote_count > 0:
                remote_idx = cursor - remote_sep - 1
                if remote_idx < remote_count:
                    self.state.cursor_instance_name = remote_instances[remote_idx][0]
                else:
                    self.state.cursor_instance_name = None
            else:
                self.state.cursor_instance_name = None
        else:
            self.state.cursor = 0
            self.state.cursor_instance_name = None

        # Empty state - no instances (neither enabled, stopped, nor remote)
        if len(enabled_instances) == 0 and stopped_count == 0 and remote_count == 0:
            lines.append('')
            lines.append(f"{FG_GRAY}No instances - Press Tab → LAUNCH{RESET}")
            lines.append('')
            # Pad to instance_rows
            while len(lines) < instance_rows:
                lines.append('')
        else:
            # Calculate total display items: enabled + stopped section + remote section
            display_count = len(enabled_instances)
            if stopped_count > 0:
                display_count += 1  # stopped separator row
                if self.state.show_stopped:
                    display_count += stopped_count
            if remote_count > 0:
                display_count += 1  # remote separator row
                if self.state.show_remote:
                    display_count += remote_count

            # Calculate visible window
            max_scroll = max(0, display_count - instance_rows)
            self.state.instance_scroll_pos = max(0, min(self.state.instance_scroll_pos, max_scroll))

            # Calculate dynamic name column width based on actual names
            all_for_width = enabled_instances + (stopped_instances if self.state.show_stopped else [])
            if self.state.show_remote:
                all_for_width += remote_instances
            max_instance_name_len = max((len(name) for name, _ in all_for_width), default=0)
            # Check if any instance has badges
            from ..core.instances import is_external_sender
            has_background = any(info.get('data', {}).get('background', False) for _, info in all_for_width)
            has_external = any(is_external_sender(info.get('data', {})) for _, info in all_for_width)
            # Calculate max badge length: " [headless]" (11) + " [external]" (11) = 22
            badge_len = 0
            if has_background: badge_len += 11
            if has_external: badge_len += 11
            # Add space for state symbol on cursor row (2 chars: " +")
            name_col_width = max_instance_name_len + badge_len + 2
            # Set bounds: min 20, max based on terminal width
            # Reserve: 2 (icon) + 10 (age) + 2 (sep) + 30 (desc min) = 44
            max_name_width = max(20, width - 44)
            name_col_width = max(20, min(name_col_width, max_name_width))

            # Build display rows
            visible_start = self.state.instance_scroll_pos
            visible_end = min(visible_start + instance_rows, display_count)

            # If only 1 item would be hidden, show it instead of scroll indicator
            if visible_start == 1:
                visible_start = 0
            if display_count - visible_end == 1:
                visible_end = display_count

            for display_idx in range(visible_start, visible_end):
                # Determine what this display row represents
                if display_idx < len(enabled_instances):
                    # Enabled instance
                    name, info = enabled_instances[display_idx]
                    line = self._render_instance_row(name, info, display_idx, name_col_width, width)
                    lines.append(line)
                elif stopped_sep >= 0 and display_idx == stopped_sep:
                    # Stopped separator row
                    is_cursor = (display_idx == self.state.cursor)
                    arrow = "▼" if self.state.show_stopped else "▶"
                    sep_text = f" stopped ({stopped_count}) {arrow} "
                    pad_len = (width - len(sep_text) - 2) // 2
                    sep_line = f"{'─' * pad_len}{sep_text}{'─' * pad_len}"
                    if is_cursor:
                        line = f"{BG_CHARCOAL}{FG_GRAY}{sep_line}{RESET}"
                        line = bg_ljust(line, width, BG_CHARCOAL)
                    else:
                        line = f"{FG_GRAY}{sep_line}{RESET}"
                    lines.append(truncate_ansi(line, width))
                elif self.state.show_stopped and stopped_count > 0 and display_idx < stopped_sep + 1 + stopped_count:
                    # Stopped instance (only when expanded)
                    stopped_idx = display_idx - stopped_sep - 1
                    if 0 <= stopped_idx < stopped_count:
                        name, info = stopped_instances[stopped_idx]
                        line = self._render_instance_row(name, info, display_idx, name_col_width, width)
                        lines.append(line)
                elif remote_sep >= 0 and display_idx == remote_sep:
                    # Relay separator row (no dot here - dot is in top bar)
                    is_cursor = (display_idx == self.state.cursor)
                    arrow = "▼" if self.state.show_remote else "▶"

                    # Build sync status when expanded: relay (ALIAS:1m, ALIAS:2s) ▼
                    if self.state.show_remote and self.state.device_sync_times:
                        # Build device_id -> alias mapping from remote instances
                        device_aliases = {}
                        for name, info in remote_instances:
                            origin_device = info.get('data', {}).get('origin_device_id', '')
                            if origin_device and ':' in name:
                                alias = name.rsplit(':', 1)[1]
                                device_aliases[origin_device] = alias

                        sync_parts = []
                        for device, sync_time in sorted(self.state.device_sync_times.items()):
                            if sync_time:
                                sync_age = time.time() - sync_time
                                alias = device_aliases.get(device, device[:4].upper())
                                color = get_device_sync_color(sync_age)
                                sync_parts.append(f"{color}{alias}:{format_age(sync_age)}{FG_GRAY}")

                        if sync_parts:
                            sep_text = f" relay ({', '.join(sync_parts)}) {arrow} "
                        else:
                            sep_text = f" relay ({remote_count}) {arrow} "
                    else:
                        sep_text = f" relay ({remote_count}) {arrow} "

                    pad_len = max(0, (width - ansi_len(sep_text) - 2) // 2)
                    sep_line = f"{'─' * pad_len}{sep_text}{'─' * pad_len}"
                    if is_cursor:
                        line = f"{BG_CHARCOAL}{FG_GRAY}{sep_line}{RESET}"
                        line = bg_ljust(line, width, BG_CHARCOAL)
                    else:
                        line = f"{FG_GRAY}{sep_line}{RESET}"
                    lines.append(truncate_ansi(line, width))
                elif self.state.show_remote and remote_count > 0:
                    # Remote instance (only when expanded)
                    remote_idx = display_idx - remote_sep - 1
                    if 0 <= remote_idx < remote_count:
                        name, info = remote_instances[remote_idx]
                        line = self._render_instance_row(name, info, display_idx, name_col_width, width, is_remote=True)
                        lines.append(line)

            # Add scroll indicators if needed
            if display_count > instance_rows:
                # If cursor will conflict with indicator, move cursor line first
                if visible_start > 0 and self.state.cursor == visible_start:
                    # Save cursor line (at position 0), move to position 1
                    cursor_line = lines[0] if lines else ""
                    lines[0] = lines[1] if len(lines) > 1 else ""
                    if len(lines) > 1:
                        lines[1] = cursor_line

                if visible_end < display_count and self.state.cursor == visible_end - 1:
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
                lines.append('')

        # Separator
        lines.append(separator_line(width))

        # Instance detail section (if active) - render ABOVE messages
        detail_rows = 0
        if self.state.show_instance_detail:
            detail_lines = self.build_instance_detail(self.state.show_instance_detail, width)
            lines.extend(detail_lines)
            detail_rows = len(detail_lines)
            # Separator after detail
            lines.append(separator_line(width))
            detail_rows += 1  # Include separator in count

        # Calculate remaining message rows (subtract detail from message budget)
        actual_message_rows = message_rows - detail_rows

        # Messages - Slack-style format with sender on separate line
        if self.state.messages and actual_message_rows > 0:
            all_wrapped_lines = []

            # Get instance read positions for read receipt calculation
            instance_reads = {}
            remote_instances = set()
            remote_msg_ts = {}
            try:
                from ..core.db import get_db
                conn = get_db()
                rows = conn.execute("SELECT name, last_event_id, origin_device_id FROM instances").fetchall()
                instance_reads = {row['name']: row['last_event_id'] for row in rows}
                remote_instances = {row['name'] for row in rows if row['origin_device_id']}
                # Get max msg_ts for remote instances from their status events
                for inst_name in remote_instances:
                    row = conn.execute("""
                        SELECT json_extract(data, '$.msg_ts') as msg_ts
                        FROM events WHERE type = 'status' AND instance = ?
                          AND json_extract(data, '$.msg_ts') IS NOT NULL
                        ORDER BY id DESC LIMIT 1
                    """, (inst_name,)).fetchone()
                    if row and row['msg_ts']:
                        remote_msg_ts[inst_name] = row['msg_ts']
            except Exception:
                pass  # No read receipts if DB query fails

            for time_str, sender, message, delivered_to, event_id in self.state.messages:
                # Format timestamp
                dt = parse_iso_timestamp(time_str) if 'T' in time_str else None
                display_time = dt.strftime('%H:%M') if dt else (time_str[:5] if len(time_str) >= 5 else time_str)

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
                        if recipient in remote_instances:
                            has_read = remote_msg_ts.get(recipient, '') >= time_str
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

                # Header line: timestamp + sender + recipients (truncated to width)
                header = f"{FG_GRAY}{display_time}{RESET} {BOLD}{sender}{RESET}{recipient_str}"
                header = truncate_ansi(header, width)
                all_wrapped_lines.append(header)

                # Replace literal newlines with space for preview
                display_message = message.replace('\n', ' ')

                # Bold @mentions in message (e.g., @name or @name:DEVICE)
                if '@' in display_message:
                    display_message = re.sub(r'(@[\w\-_:]+)', f'{BOLD}\\1{RESET}{FG_LIGHTGRAY}', display_message)

                # Message lines with indent (6 spaces for visual balance)
                indent = '      '
                max_msg_len = width - len(indent)

                # Wrap message text
                if max_msg_len > 0:
                    wrapper = AnsiTextWrapper(width=max_msg_len)
                    wrapped = wrapper.wrap(display_message)

                    # All message lines indented uniformly
                    for wrapped_line in wrapped:
                        line = f"{indent}{FG_LIGHTGRAY}{wrapped_line}{RESET}"
                        all_wrapped_lines.append(line)
                else:
                    # Fallback if width too small
                    all_wrapped_lines.append(f"{indent}{FG_LIGHTGRAY}{display_message[:width-len(indent)]}{RESET}")

                # Blank line after each message (for separation)
                all_wrapped_lines.append('')

            # Take last N lines to fit available space (mid-message truncation)
            visible_lines = all_wrapped_lines[-actual_message_rows:] if len(all_wrapped_lines) > actual_message_rows else all_wrapped_lines
            lines.extend(visible_lines)
        else:
            # ASCII art logo
            lines.append(f"{FG_GRAY}No messages yet{RESET}")
            lines.append('')
            lines.append(f"{FG_ORANGE}     ╦ ╦╔═╗╔═╗╔╦╗{RESET}")
            lines.append(f"{FG_ORANGE}     ╠═╣║  ║ ║║║║{RESET}")
            lines.append(f"{FG_ORANGE}     ╩ ╩╚═╝╚═╝╩ ╩{RESET}")

        # Calculate how many lines are used before input (instances + detail + messages + separators)
        lines_before_input = len(lines)

        # Reserve space for input at bottom: input_rows + 2 separators (before + after)
        input_section_height = input_rows + 2
        max_lines_before_input = height - input_section_height

        # Truncate message/detail area if it overflows, keeping input visible
        if lines_before_input > max_lines_before_input:
            lines = lines[:max_lines_before_input]

        # Pad to fill space before input
        while len(lines) < max_lines_before_input:
            lines.append('')

        # Separator before input
        lines.append(separator_line(width))

        # Input area (auto-wrapped) - at bottom, always visible
        input_lines = self.render_wrapped_input(width, input_rows)
        lines.extend(input_lines)

        # Separator after input
        lines.append(separator_line(width))

        return lines


    def _get_display_lists(self):
        """Build enabled/stopped/remote instance lists for cursor navigation"""
        from ..core.instances import is_remote_instance

        all_instances = sorted(
            self.state.instances.items(),
            key=lambda x: -x[1]['data'].get('created_at', 0.0)
        )

        # Separate local vs remote
        local_instances = [(n, i) for n, i in all_instances if not is_remote_instance(i.get('data', {}))]
        remote_instances = [(n, i) for n, i in all_instances if is_remote_instance(i.get('data', {}))]
        # Sort remote: enabled first, then by created_at (must match build())
        remote_instances.sort(key=lambda x: (not x[1].get('enabled', False), -x[1]['data'].get('created_at', 0.0)))

        enabled = [(n, i) for n, i in local_instances if i.get('enabled', False)]
        stopped = [(n, i) for n, i in local_instances if not i.get('enabled', False)]
        stopped_count = len(stopped)
        remote_count = len(remote_instances)

        # Calculate display count: enabled + stopped section + remote section
        display_count = len(enabled)
        if stopped_count > 0:
            display_count += 1  # stopped separator
            if self.state.show_stopped:
                display_count += stopped_count
        if remote_count > 0:
            display_count += 1  # remote separator
            if self.state.show_remote:
                display_count += remote_count

        return enabled, stopped, remote_instances, display_count

    def _get_instance_at_cursor(self, enabled, stopped, remote):
        """Get (instance, is_remote) at cursor, or (None, False) if on separator"""
        stopped_count = len(stopped)
        remote_count = len(remote)

        # Calculate section boundaries
        enabled_end = len(enabled)
        stopped_sep = enabled_end if stopped_count > 0 else -1
        stopped_end = stopped_sep + 1 + stopped_count if stopped_count > 0 and self.state.show_stopped else stopped_sep + 1 if stopped_count > 0 else enabled_end
        remote_sep_pos = stopped_end if remote_count > 0 else -1

        cursor = self.state.cursor

        # Enabled section
        if cursor < enabled_end:
            return enabled[cursor], False

        # Stopped separator
        if stopped_count > 0 and cursor == stopped_sep:
            return None, False

        # Stopped instances (if expanded)
        if stopped_count > 0 and self.state.show_stopped:
            stopped_start = stopped_sep + 1
            if stopped_start <= cursor < stopped_start + stopped_count:
                return stopped[cursor - stopped_start], False

        # Remote separator
        if remote_count > 0 and cursor == remote_sep_pos:
            return None, False

        # Remote instances (if expanded)
        if remote_count > 0 and self.state.show_remote:
            remote_start = remote_sep_pos + 1
            if remote_start <= cursor < remote_start + remote_count:
                return remote[cursor - remote_start], True

        return None, False

    def _get_separator_positions(self, enabled, stopped, remote):
        """Calculate separator positions for stopped and remote sections"""
        stopped_count = len(stopped)
        remote_count = len(remote)

        # Stopped separator is right after enabled
        stopped_sep = len(enabled) if stopped_count > 0 else -1

        # Remote separator is after stopped section (expanded or collapsed)
        if remote_count > 0:
            if stopped_count > 0:
                if self.state.show_stopped:
                    remote_sep = stopped_sep + 1 + stopped_count
                else:
                    remote_sep = stopped_sep + 1
            else:
                remote_sep = len(enabled)
        else:
            remote_sep = -1

        return stopped_sep, remote_sep

    def handle_key(self, key: str):
        """Handle keys in Manage mode"""
        # Build display lists
        enabled, stopped, remote, display_count = self._get_display_lists()
        stopped_count = len(stopped)
        remote_count = len(remote)
        stopped_sep, remote_sep = self._get_separator_positions(enabled, stopped, remote)

        if key == 'UP':
            if display_count > 0 and self.state.cursor > 0:
                self.state.cursor -= 1
                inst, is_remote = self._get_instance_at_cursor(enabled, stopped, remote)
                self.state.cursor_instance_name = inst[0] if inst else None
                self.tui.clear_all_pending_confirmations()
                self.state.show_instance_detail = None
                self.sync_scroll_to_cursor()
        elif key == 'DOWN':
            if display_count > 0 and self.state.cursor < display_count - 1:
                self.state.cursor += 1
                inst, is_remote = self._get_instance_at_cursor(enabled, stopped, remote)
                self.state.cursor_instance_name = inst[0] if inst else None
                self.tui.clear_all_pending_confirmations()
                self.state.show_instance_detail = None
                self.sync_scroll_to_cursor()
        elif key == '@':
            self.tui.clear_all_pending_confirmations()
            inst, is_remote = self._get_instance_at_cursor(enabled, stopped, remote)
            if inst:
                name, _ = inst
                mention = f"@{name} "  # Name already includes :DEVICE suffix for remote
                if mention not in self.state.message_buffer:
                    self.state.message_buffer, self.state.message_cursor_pos = text_input_insert(
                        self.state.message_buffer, self.state.message_cursor_pos, mention
                    )
        elif key == 'SPACE':
            self.tui.clear_all_pending_confirmations()
            # Add space to message buffer at cursor position
            self.state.message_buffer, self.state.message_cursor_pos = text_input_insert(
                self.state.message_buffer, self.state.message_cursor_pos, ' '
            )
        elif key == 'LEFT':
            self.tui.clear_all_pending_confirmations()
            # Move cursor left in message buffer
            self.state.message_cursor_pos = text_input_move_left(self.state.message_cursor_pos)
        elif key == 'RIGHT':
            self.tui.clear_all_pending_confirmations()
            # Move cursor right in message buffer
            self.state.message_cursor_pos = text_input_move_right(self.state.message_buffer, self.state.message_cursor_pos)
        elif key == 'ESC':
            # Clear everything: message buffer, detail view, and confirmations
            self.state.message_buffer = ""
            self.state.message_cursor_pos = 0
            self.state.show_instance_detail = None
            self.tui.clear_all_pending_confirmations()
        elif key == 'BACKSPACE':
            self.tui.clear_all_pending_confirmations()
            # Delete character before cursor in message buffer
            self.state.message_buffer, self.state.message_cursor_pos = text_input_backspace(
                self.state.message_buffer, self.state.message_cursor_pos
            )
        elif key == 'ENTER':
            # Clear stop all and reset confirmations (toggle handled separately below)
            self.tui.clear_pending_confirmations_except('toggle')

            # Smart Enter: send message if text exists, otherwise toggle instances
            if self.state.message_buffer.strip():
                # Send message using cmd_send for consistent validation and error handling
                # Dim input during send (visual feedback without flash)
                self.state.send_state = 'sending'
                self.state.frame_dirty = True
                self.tui.render()  # Force immediate display
                try:
                    message = self.state.message_buffer.strip()
                    result = cmd_send([message])
                    if result == 0:
                        # Brief bright prefix, then clear
                        self.state.send_state = 'sent'
                        self.state.send_state_until = time.time() + 0.1
                        # Clear message buffer and cursor
                        self.state.message_buffer = ""
                        self.state.message_cursor_pos = 0
                    else:
                        self.state.send_state = None
                        self.tui.flash_error("Send failed")
                except Exception as e:
                    self.state.send_state = None
                    self.tui.flash_error(f"Error: {str(e)}")
            else:
                # Check if on stopped separator - toggle show_stopped
                if stopped_sep >= 0 and self.state.cursor == stopped_sep:
                    self.state.show_stopped = not self.state.show_stopped
                    self.state.show_stopped_user_set = True
                    return

                # Check if on remote separator - toggle show_remote
                if remote_sep >= 0 and self.state.cursor == remote_sep:
                    self.state.show_remote = not self.state.show_remote
                    self.state.show_remote_user_set = True
                    return

                # Get instance at cursor
                inst, is_remote = self._get_instance_at_cursor(enabled, stopped, remote)
                if not inst:
                    return

                name, info = inst

                # Remote instances: toggle via control event
                if is_remote:
                    from ..relay import send_control
                    # Extract device short ID from name (format: name:DEVICE)
                    if ':' in name:
                        base_name, device_short = name.rsplit(':', 1)
                        is_enabled = info.get('enabled', False)
                        action = "stop" if is_enabled else "start"

                        # Get status color for flash message
                        status = info.get('status', "unknown")
                        color = STATUS_FG.get(status, FG_WHITE)

                        # Two-step confirmation for remote toggle
                        if self.state.pending_toggle == name and (time.time() - self.state.pending_toggle_time) <= self.tui.CONFIRMATION_TIMEOUT:
                            # Execute remote toggle
                            if send_control(action, base_name, device_short):
                                verb = "Stopped" if action == "stop" else "Started"
                                self.tui.flash(f"{verb} hcom for {color}{name}{RESET}")
                                self.state.completed_toggle = name
                                self.state.completed_toggle_time = time.time()
                                self.tui.load_status()
                            else:
                                self.tui.flash_error(f"Failed to {action} remote instance")
                            self.state.pending_toggle = None
                            self.state.show_instance_detail = None
                        else:
                            # First press - request confirmation + show detail
                            self.state.pending_toggle = name
                            self.state.pending_toggle_time = time.time()
                            self.state.show_instance_detail = name
                            name_colored = f"{color}{name}{FG_WHITE}"
                            self.tui.flash(f"Confirm {action} {name_colored}? (press Enter again)", duration=self.tui.CONFIRMATION_TIMEOUT, color='white')
                    else:
                        # Can't parse device ID, just show detail
                        self.state.show_instance_detail = name
                    return

                # Local instance - toggle with two-step confirmation
                is_enabled = info['data'].get('enabled', False)
                action = "start" if not is_enabled else "stop"

                # Get status color for name
                status = info.get('status', "unknown")
                color = STATUS_FG.get(status, FG_WHITE)

                # Light green coloring for message delivery (active with deliver token)
                status_context = info.get('data', {}).get('status_context', '')
                if status == 'active' and status_context.startswith('deliver:'):
                    color = FG_DELIVER

                # Check if confirming previous toggle
                if self.state.pending_toggle == name and (time.time() - self.state.pending_toggle_time) <= self.tui.CONFIRMATION_TIMEOUT:
                    # Execute toggle (confirmation received)
                    try:
                        if is_enabled:
                            # DEBUG: Log TUI toggle action
                            from ..hooks.utils import log_hook_error
                            parent_name = info['data'].get('parent_name', 'none')
                            log_hook_error(f'tui:toggle_stop', f'TUI calling cmd_stop for {name} (parent={parent_name}, enabled={is_enabled})')

                            # Suppress CLI output to prevent TUI corruption
                            with _suppress_output():
                                cmd_stop([name])
                            self.tui.flash(f"Stopped hcom for {color}{name}{RESET}")
                            self.state.completed_toggle = name
                            self.state.completed_toggle_time = time.time()
                        else:
                            # Suppress CLI output to prevent TUI corruption
                            with _suppress_output():
                                cmd_start([name])
                            self.tui.flash(f"Started hcom for {color}{name}{RESET}")
                            self.state.completed_toggle = name
                            self.state.completed_toggle_time = time.time()
                        self.tui.load_status()
                    except Exception as e:
                        self.tui.flash_error(f"Error: {str(e)}")
                    finally:
                        self.state.pending_toggle = None
                        self.state.show_instance_detail = None
                else:
                    # Show confirmation (first press) + show detail view
                    self.state.pending_toggle = name
                    self.state.pending_toggle_time = time.time()
                    self.state.show_instance_detail = name  # Also show detail
                    # Name with status color, action is plain text (no color clash)
                    name_colored = f"{color}{name}{FG_WHITE}"
                    # Flash with 10s timeout
                    self.tui.flash(f"Confirm {action} {name_colored}? (press Enter again)", duration=self.tui.CONFIRMATION_TIMEOUT, color='white')

        elif key == 'CTRL_A':
            # Check state before clearing
            is_confirming = self.state.pending_stop_all and (time.time() - self.state.pending_stop_all_time) <= self.tui.CONFIRMATION_TIMEOUT
            self.tui.clear_pending_confirmations_except('stop_all')

            # Two-step confirmation for stop all
            if is_confirming:
                # Execute stop all (confirmation received)
                self.tui.stop_all_instances()
                self.state.pending_stop_all = False
            else:
                # Show confirmation (first press) - 10s duration
                self.state.pending_stop_all = True
                self.state.pending_stop_all_time = time.time()
                self.tui.flash(f"{FG_WHITE}Confirm stop all instances? (press Ctrl+A again){RESET}", duration=self.tui.CONFIRMATION_FLASH_DURATION, color='white')

        elif key == 'CTRL_R':
            # Check state before clearing
            is_confirming = self.state.pending_reset and (time.time() - self.state.pending_reset_time) <= self.tui.CONFIRMATION_TIMEOUT
            self.tui.clear_pending_confirmations_except('reset')

            # Two-step confirmation for reset
            if is_confirming:
                # Execute reset (confirmation received)
                self.tui.reset_events()
                self.state.pending_reset = False
            else:
                # Show confirmation (first press)
                self.state.pending_reset = True
                self.state.pending_reset_time = time.time()
                self.tui.flash(f"{FG_WHITE}Confirm clear & archive (conversation + instance list)? (press Ctrl+R again){RESET}", duration=self.tui.CONFIRMATION_FLASH_DURATION, color='white')

        elif key == '\n':
            # Handle pasted newlines - insert literally
            self.tui.clear_all_pending_confirmations()
            self.state.message_buffer, self.state.message_cursor_pos = text_input_insert(
                self.state.message_buffer, self.state.message_cursor_pos, '\n'
            )

        elif key and len(key) == 1 and key.isprintable():
            self.tui.clear_all_pending_confirmations()
            # Insert printable characters at cursor position
            self.state.message_buffer, self.state.message_cursor_pos = text_input_insert(
                self.state.message_buffer, self.state.message_cursor_pos, key
            )

    def calculate_layout(self, height: int, width: int) -> tuple[int, int, int]:
        """Calculate instance/message/input row allocation"""
        from ..core.instances import is_remote_instance

        # Dynamic input area based on buffer size
        input_rows = calculate_text_input_rows(self.state.message_buffer, width)
        # Space budget
        separator_rows = 3  # One separator between instances and messages, one before input, one after input
        min_instance_rows = 3

        available = height - input_rows - separator_rows

        # Calculate display count based on current collapse state
        all_instances = list(self.state.instances.values())
        local_instances = [i for i in all_instances if not is_remote_instance(i.get('data', {}))]
        remote_instances = [i for i in all_instances if is_remote_instance(i.get('data', {}))]

        enabled_count = sum(1 for i in local_instances if i.get('enabled', False))
        stopped_count = len(local_instances) - enabled_count
        remote_count = len(remote_instances)

        # Build display count: enabled + stopped section + remote section
        display_count = enabled_count
        if stopped_count > 0:
            display_count += 1  # stopped separator
            if self.state.show_stopped:
                display_count += stopped_count
        if remote_count > 0:
            display_count += 1  # remote separator
            if self.state.show_remote:
                display_count += remote_count

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
        if self.state.cursor < self.state.instance_scroll_pos:
            self.state.instance_scroll_pos = self.state.cursor
        # Scroll down if cursor moved below visible window
        elif self.state.cursor >= self.state.instance_scroll_pos + visible_instance_rows:
            self.state.instance_scroll_pos = self.state.cursor - visible_instance_rows + 1

    def render_wrapped_input(self, width: int, input_rows: int) -> List[str]:
        """Render message input (delegates to shared helper)"""
        return render_text_input(
            self.state.message_buffer,
            self.state.message_cursor_pos,
            width,
            input_rows,
            prefix="> ",
            send_state=self.state.send_state
        )

    def build_instance_detail(self, name: str, width: int) -> List[str]:
        """Build instance metadata display (similar to hcom list --verbose)"""
        import os
        import time
        from ..core.instances import is_external_sender, is_remote_instance

        lines = []

        # Get instance data
        if name not in self.state.instances:
            return [f"{FG_GRAY}Instance not found{RESET}"]

        info = self.state.instances[name]
        data = info['data']

        # Get status color for name (same as flash message)
        status = info.get('status', "unknown")
        color = STATUS_FG.get(status, FG_WHITE)

        # Light green coloring for message delivery (active with deliver token)
        status_context = data.get('status_context', '')
        if status == 'active' and status_context.startswith('deliver:'):
            color = FG_DELIVER

        # Header: bold colored name (badges already shown in instance list)
        header = f"{BOLD}{color}{name}{RESET}"
        lines.append(truncate_ansi(header, width))

        if is_remote_instance(data):
            # Remote instance: show device/sync info plus available details
            origin_device = data.get("origin_device_id", "")
            device_short = origin_device[:8] if origin_device else "(unknown)"

            # Get device sync time
            sync_time = self.state.device_sync_times.get(origin_device, 0)
            sync_str = f"{format_age(time.time() - sync_time)} ago" if sync_time else "never"

            lines.append(truncate_ansi(f"  device:     {device_short}", width))
            lines.append(truncate_ansi(f"  last_sync:  {sync_str}", width))

            # Show available remote instance details
            session_id = data.get("session_id") or "(none)"
            lines.append(truncate_ansi(f"  session_id: {session_id}", width))

            parent = data.get("parent_name")
            if parent:
                lines.append(truncate_ansi(f"  parent:     {parent}", width))

            directory = data.get("directory")
            if directory:
                lines.append(truncate_ansi(f"  directory:  {shorten_path(directory)}", width))

            # Format status_time
            status_time = data.get("status_time", 0)
            if status_time:
                lines.append(truncate_ansi(f"  status_at:  {format_age(time.time() - status_time)} ago", width))

            # Show timeout and last_stop (now synced)
            timeout = data.get("wait_timeout", 1800)
            lines.append(truncate_ansi(f"  timeout:    {timeout}s", width))

            last_stop = data.get("last_stop", 0)
            if last_stop:
                lines.append(truncate_ansi(f"  last_stop:  {format_age(time.time() - last_stop)} ago", width))
        else:
            # Local instance: show full details
            session_id = data.get("session_id") or "None"
            directory = data.get("directory") or "(none)"
            timeout = data.get("wait_timeout", 1800)
            parent = data.get("parent_name") or None

            # Format paths (shorten with ~)
            directory = shorten_path(directory) if directory != "(none)" else directory
            log_file = shorten_path(data.get("background_log_file"))
            transcript = shorten_path(data.get("transcript_path")) or "(none)"

            # Format created_at timestamp
            created_ts = data.get("created_at")
            created = f"{format_age(time.time() - created_ts)} ago" if created_ts else "(unknown)"

            # Format tcp_mode
            tcp = "instant" if data.get("tcp_mode") else "polling"

            # Build detail lines (truncated to terminal width)
            lines.append(truncate_ansi(f"  session_id:   {session_id}", width))
            lines.append(truncate_ansi(f"  created:      {created}", width))
            lines.append(truncate_ansi(f"  directory:    {directory}", width))
            lines.append(truncate_ansi(f"  timeout:      {timeout}s", width))

            if parent:
                agent_id = data.get("agent_id") or "(none)"
                lines.append(truncate_ansi(f"  parent:       {parent}", width))
                lines.append(truncate_ansi(f"  agent_id:     {agent_id}", width))

            lines.append(truncate_ansi(f"  tcp_mode:     {tcp}", width))

            if log_file:
                lines.append(truncate_ansi(f"  headless log: {log_file}", width))

            lines.append(truncate_ansi(f"  transcript:   {transcript}", width))

        return lines
