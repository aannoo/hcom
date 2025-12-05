"""Launch mode screen implementation"""
from __future__ import annotations
from typing import List, TYPE_CHECKING
import os
import re
import time

if TYPE_CHECKING:
    from ..ui import HcomTUI, UIState

# Import types
from ..ui import Field, LaunchField, Mode

# Import rendering utilities
from ..ui import (
    ansi_len, ansi_ljust, bg_ljust, truncate_ansi, get_terminal_size,
    separator_line,
)

# Import input utilities
from ..ui import (
    render_text_input, calculate_text_input_rows,
    text_input_insert, text_input_backspace,
    text_input_move_left, text_input_move_right
)

# Import from shared and api
from ..shared import (
    RESET, BOLD, DIM,
    FG_WHITE, FG_BLACK, FG_GRAY, FG_ORANGE, FG_CYAN, FG_RED,
    BG_CHARCOAL, BG_ORANGE,
    BOX_H,
    DEFAULT_CONFIG_DEFAULTS,
)
from ..ui import CONFIG_DEFAULTS, CONFIG_FIELD_OVERRIDES, FG_CLAUDE_ORANGE, FG_CUSTOM_ENV
from ..api import (
    get_config, reload_config, cmd_launch,
    resolve_claude_args
)
from ..commands.admin import reset_config


class LaunchScreen:
    """Launch mode: form-based instance creation"""

    _claude_defaults_cache = None  # Class-level cache for claude defaults

    def __init__(self, state: UIState, tui: HcomTUI):
        self.state = state  # Shared state (explicit dependency)
        self.tui = tui      # For commands only (flash, config loading, cmd_launch)

    def _get_claude_defaults(self) -> tuple[str, str, str, bool]:
        """Get (prompt, system, append, background) defaults from HCOM_CLAUDE_ARGS. Cached."""
        if LaunchScreen._claude_defaults_cache is None:
            claude_args_default = CONFIG_DEFAULTS.get('HCOM_CLAUDE_ARGS', '')
            spec = resolve_claude_args(None, claude_args_default if claude_args_default else None)
            LaunchScreen._claude_defaults_cache = (
                spec.positional_tokens[0] if spec.positional_tokens else "",
                spec.user_system or "",
                spec.user_append or "",
                spec.is_background,
            )
        return LaunchScreen._claude_defaults_cache

    def build(self, height: int, width: int) -> List[str]:
        """Build launch screen with expandable sections"""
        # Calculate editor space upfront (reserves bottom of screen)
        field_info = self.get_current_field_info()

        # Calculate dynamic editor rows (like manage screen)
        if field_info:
            field_key, field_value, cursor_pos = field_info
            editor_content_rows = calculate_text_input_rows(field_value, width)
            editor_rows = editor_content_rows + 4  # +4 for separator, header, blank line, separator
            separator_rows = 0  # Editor includes separator
        else:
            editor_rows = 0
            editor_content_rows = 0
            separator_rows = 1  # Need separator when no editor

        form_height = height - editor_rows - separator_rows

        lines = []
        selected_field_start_line = None  # Track which line has the selected field

        lines.append('')  # Top padding

        # Count field (with left padding)
        count_selected = (self.state.launch_field == LaunchField.COUNT)
        if count_selected:
            selected_field_start_line = len(lines)
            line = f"  {BG_CHARCOAL}{FG_WHITE}{BOLD}\u25b8 Count:{RESET}{BG_CHARCOAL} {FG_ORANGE}{self.state.launch_count}{RESET}{BG_CHARCOAL}  {FG_GRAY}\u2022 \u2190\u2192 adjust{RESET}"
            lines.append(bg_ljust(line, width, BG_CHARCOAL))
        else:
            lines.append(f"  {FG_WHITE}Count:{RESET} {FG_ORANGE}{self.state.launch_count}{RESET}")

        # Launch button (with left padding)
        launch_selected = (self.state.launch_field == LaunchField.LAUNCH_BTN)
        if launch_selected:
            selected_field_start_line = len(lines)
            lines.append(f"  {BG_ORANGE}{FG_BLACK}{BOLD} \u25b6 Launch \u23ce {RESET}")
            # Show cwd when launch button is selected
            cwd = os.getcwd()
            max_cwd_width = width - 10  # Leave margin
            if len(cwd) > max_cwd_width:
                cwd = '\u2026' + cwd[-(max_cwd_width - 1):]
            lines.append(f"  {BG_CHARCOAL}{FG_GRAY} \u2022 {FG_WHITE}{cwd} {RESET}")
        else:
            lines.append(f"  {FG_GRAY}\u25b6{RESET} {FG_ORANGE}{BOLD}Launch{RESET}")

        lines.append('')  # Spacer
        lines.append(f"{DIM}{FG_GRAY}{BOX_H * width}{RESET}")  # Separator (dim)
        lines.append('')  # Spacer

        # Claude section header (with left padding)
        claude_selected = (self.state.launch_field == LaunchField.CLAUDE_SECTION and self.state.claude_cursor == -1)
        expand_marker = '\u25bc' if self.state.claude_expanded else '\u25b6'
        claude_fields = self.build_claude_fields()
        # Count fields modified from defaults
        claude_set = 0
        default_prompt, default_system, default_append, default_background = self._get_claude_defaults()

        if self.state.launch_background != default_background:
            claude_set += 1
        if self.state.launch_prompt != default_prompt:
            claude_set += 1
        if self.state.launch_system_prompt != default_system:
            claude_set += 1
        if self.state.launch_append_system_prompt != default_append:
            claude_set += 1
        # claude_args: check if raw value differs from default (normalize quotes)
        claude_args_val = self.state.config_edit.get('HCOM_CLAUDE_ARGS', '').strip().strip("'\"")
        claude_args_default_normalized = CONFIG_DEFAULTS.get('HCOM_CLAUDE_ARGS', '').strip().strip("'\"")
        if claude_args_val != claude_args_default_normalized:
            claude_set += 1
        claude_total = len(claude_fields)
        claude_count = f" \u2022 {claude_set}/{claude_total}"
        if claude_selected:
            selected_field_start_line = len(lines)
            claude_action = "\u2190 collapse" if self.state.claude_expanded else "\u2192 expand"
            claude_hint = f"{claude_count} \u2022 {claude_action}"
            line = f"  {BG_CHARCOAL}{FG_CLAUDE_ORANGE}{BOLD}{expand_marker} Claude{RESET}{BG_CHARCOAL}  {FG_GRAY}{claude_hint}{RESET}"
            lines.append(bg_ljust(line, width, BG_CHARCOAL))
        else:
            lines.append(f"  {FG_CLAUDE_ORANGE}{BOLD}{expand_marker} Claude{RESET}{FG_GRAY}{claude_count}{RESET}")

        # Preview modified fields when collapsed, or show description if none
        if not self.state.claude_expanded:
            if claude_set > 0:
                previews = []
                if self.state.launch_background != default_background:
                    previews.append("background: true" if self.state.launch_background else "background: false")
                if self.state.launch_prompt != default_prompt:
                    prompt_str = str(self.state.launch_prompt) if self.state.launch_prompt else ""
                    prompt_preview = prompt_str[:20] + "..." if len(prompt_str) > 20 else prompt_str
                    previews.append(f'prompt: "{prompt_preview}"')
                if self.state.launch_system_prompt != default_system:
                    sys_str = str(self.state.launch_system_prompt) if self.state.launch_system_prompt else ""
                    sys_preview = sys_str[:20] + "..." if len(sys_str) > 20 else sys_str
                    previews.append(f'system: "{sys_preview}"')
                if self.state.launch_append_system_prompt != default_append:
                    append_str = str(self.state.launch_append_system_prompt) if self.state.launch_append_system_prompt else ""
                    append_preview = append_str[:20] + "..." if len(append_str) > 20 else append_str
                    previews.append(f'append: "{append_preview}"')
                if claude_args_val != claude_args_default_normalized:
                    args_str = str(claude_args_val) if claude_args_val else ""
                    args_preview = args_str[:25] + "..." if len(args_str) > 25 else args_str
                    previews.append(f'args: "{args_preview}"')
                if previews:
                    preview_text = ", ".join(previews)
                    lines.append(f"    {DIM}{FG_GRAY}{truncate_ansi(preview_text, width - 4)}{RESET}")
            else:
                lines.append(f"    {DIM}{FG_GRAY}prompt, system, append, headless, args{RESET}")

        # Claude fields (if expanded or cursor inside)
        result = self.render_section_fields(
            lines, claude_fields, self.state.claude_expanded,
            LaunchField.CLAUDE_SECTION, self.state.claude_cursor, width, FG_CLAUDE_ORANGE
        )
        if result is not None:
            selected_field_start_line = result

        # Add spacing after expanded section
        if self.state.claude_expanded:
            lines.append('')

        # HCOM section header (with left padding)
        hcom_selected = (self.state.launch_field == LaunchField.HCOM_SECTION and self.state.hcom_cursor == -1)
        expand_marker = '\u25bc' if self.state.hcom_expanded else '\u25b6'
        hcom_fields = self.build_hcom_fields()
        # Count fields modified from defaults (considering runtime behavior)
        def is_field_modified(f):
            default = CONFIG_DEFAULTS.get(f.key, '')
            if not f.value:  # Empty
                # Fields where empty reverts to default at runtime
                if f.key in ('HCOM_TERMINAL', 'HCOM_HINTS', 'HCOM_TAG', 'HCOM_TIMEOUT', 'HCOM_SUBAGENT_TIMEOUT'):
                    return False  # Empty → uses default → NOT modified
                # Fields where empty stays empty (different from default if default is non-empty)
                # HCOM_CLAUDE_ARGS: empty → "" (not default "'say hi...'") → IS modified
                return bool(default.strip().strip("'\""))  # Modified if default is non-empty
            # Has value - check if different from default
            return f.value.strip().strip("'\"") != default.strip().strip("'\"")
        hcom_set = sum(1 for f in hcom_fields if is_field_modified(f))
        hcom_total = len(hcom_fields)
        hcom_count = f" \u2022 {hcom_set}/{hcom_total}"
        if hcom_selected:
            selected_field_start_line = len(lines)
            hcom_action = "\u2190 collapse" if self.state.hcom_expanded else "\u2192 expand"
            hcom_hint = f"{hcom_count} \u2022 {hcom_action}"
            line = f"  {BG_CHARCOAL}{FG_CYAN}{BOLD}{expand_marker} HCOM{RESET}{BG_CHARCOAL}  {FG_GRAY}{hcom_hint}{RESET}"
            lines.append(bg_ljust(line, width, BG_CHARCOAL))
        else:
            lines.append(f"  {FG_CYAN}{BOLD}{expand_marker} HCOM{RESET}{FG_GRAY}{hcom_count}{RESET}")

        # Preview modified fields when collapsed, or show description if none
        if not self.state.hcom_expanded:
            if hcom_set > 0:
                previews = []
                for field in hcom_fields:
                    if is_field_modified(field):
                        val = field.value or ""
                        if field.field_type == 'checkbox':
                            val_str = "true" if val == "true" else "false"
                        else:
                            val = str(val) if val else ""
                            val_str = val[:15] + "..." if len(val) > 15 else val
                        # Shorten field names
                        short_name = field.display_name.lower().replace("hcom ", "")
                        previews.append(f'{short_name}: {val_str}')
                if previews:
                    preview_text = ", ".join(previews)
                    lines.append(f"    {DIM}{FG_GRAY}{truncate_ansi(preview_text, width - 4)}{RESET}")
            else:
                lines.append(f"    {DIM}{FG_GRAY}tag, hints, timeout, terminal{RESET}")

        # HCOM fields
        result = self.render_section_fields(
            lines, hcom_fields, self.state.hcom_expanded,
            LaunchField.HCOM_SECTION, self.state.hcom_cursor, width, FG_CYAN
        )
        if result is not None:
            selected_field_start_line = result

        # Add spacing after expanded section
        if self.state.hcom_expanded:
            lines.append('')

        # Custom Env section header (with left padding)
        custom_selected = (self.state.launch_field == LaunchField.CUSTOM_ENV_SECTION and self.state.custom_env_cursor == -1)
        expand_marker = '\u25bc' if self.state.custom_env_expanded else '\u25b6'
        custom_fields = self.build_custom_env_fields()
        custom_set = sum(1 for f in custom_fields if f.value)
        custom_total = len(custom_fields)
        custom_count = f" \u2022 {custom_set}/{custom_total}"
        if custom_selected:
            selected_field_start_line = len(lines)
            custom_action = "\u2190 collapse" if self.state.custom_env_expanded else "\u2192 expand"
            custom_hint = f"{custom_count} \u2022 {custom_action}"
            line = f"  {BG_CHARCOAL}{FG_CUSTOM_ENV}{BOLD}{expand_marker} Custom Env{RESET}{BG_CHARCOAL}  {FG_GRAY}{custom_hint}{RESET}"
            lines.append(bg_ljust(line, width, BG_CHARCOAL))
        else:
            lines.append(f"  {FG_CUSTOM_ENV}{BOLD}{expand_marker} Custom Env{RESET}{FG_GRAY}{custom_count}{RESET}")

        # Preview modified fields when collapsed, or show description if none
        if not self.state.custom_env_expanded:
            if custom_set > 0:
                previews = []
                for field in custom_fields:
                    if field.value:
                        val = str(field.value) if field.value else ""
                        val_str = val[:15] + "..." if len(val) > 15 else val
                        previews.append(f'{field.key}: {val_str}')
                if previews:
                    preview_text = ", ".join(previews)
                    lines.append(f"    {DIM}{FG_GRAY}{truncate_ansi(preview_text, width - 4)}{RESET}")
            else:
                lines.append(f"    {DIM}{FG_GRAY}arbitrary environment variables{RESET}")

        # Custom Env fields
        result = self.render_section_fields(
            lines, custom_fields, self.state.custom_env_expanded,
            LaunchField.CUSTOM_ENV_SECTION, self.state.custom_env_cursor, width, FG_CUSTOM_ENV
        )
        if result is not None:
            selected_field_start_line = result

        # Add spacing after expanded section
        if self.state.custom_env_expanded:
            lines.append('')

        # Open config in editor entry (at bottom, less prominent)
        lines.append('')  # Spacer
        editor_cmd, editor_label = self.tui.resolve_editor_command()
        editor_label_display = editor_label or 'VS Code'
        editor_available = editor_cmd is not None
        editor_selected = (self.state.launch_field == LaunchField.OPEN_EDITOR)

        if editor_selected:
            selected_field_start_line = len(lines)
            lines.append(
                bg_ljust(
                    f"  {BG_CHARCOAL}{FG_WHITE}\u2197 Open config in {editor_label_display}{RESET}"
                    f"{BG_CHARCOAL}  "
                    f"{(FG_GRAY if editor_available else FG_RED)}\u2022 "
                    f"{'enter: open' if editor_available else 'code CLI not found / set $EDITOR'}{RESET}",
                    width,
                    BG_CHARCOAL,
                )
            )
        else:
            # Less prominent when not selected
            if editor_available:
                lines.append(f"  {FG_GRAY}\u2197 Open config in {editor_label_display}{RESET}")
            else:
                lines.append(f"  {FG_GRAY}\u2197 Open config in {editor_label_display} {FG_RED}(not found){RESET}")

        # Auto-scroll to keep selected field visible
        if selected_field_start_line is not None:
            max_scroll = max(0, len(lines) - form_height)

            # Scroll up if selected field is above visible window
            if selected_field_start_line < self.state.launch_scroll_pos:
                self.state.launch_scroll_pos = selected_field_start_line
            # Scroll down if selected field is below visible window
            elif selected_field_start_line >= self.state.launch_scroll_pos + form_height:
                self.state.launch_scroll_pos = selected_field_start_line - form_height + 1

            # Clamp scroll position
            self.state.launch_scroll_pos = max(0, min(self.state.launch_scroll_pos, max_scroll))

        # Render visible window instead of truncating
        if len(lines) > form_height:
            # Extract visible slice based on scroll position
            visible_lines = lines[self.state.launch_scroll_pos:self.state.launch_scroll_pos + form_height]
            # Pad if needed (shouldn't happen, but for safety)
            while len(visible_lines) < form_height:
                visible_lines.append('')
            lines = visible_lines
        else:
            # Form fits entirely, no scrolling needed
            while len(lines) < form_height:
                lines.append('')

        # Editor (if active) - always fits because we reserved space
        if field_info:
            field_key, field_value, cursor_pos = field_info

            # Build descriptive header for each field with background
            if field_key == 'prompt':
                editor_color = FG_CLAUDE_ORANGE
                field_name = "Prompt"
                help_text = "initial prompt sent on launch"
            elif field_key == 'system_prompt':
                editor_color = FG_CLAUDE_ORANGE
                field_name = "System Prompt"
                help_text = "instructions that guide behavior"
            elif field_key == 'append_system_prompt':
                editor_color = FG_CLAUDE_ORANGE
                field_name = "Append System Prompt"
                help_text = "appends to Claude Code's default system prompt"
            elif field_key == 'HCOM_CLAUDE_ARGS':
                editor_color = FG_CLAUDE_ORANGE
                field_name = "Claude Args"
                help_text = "raw flags passed to Claude CLI"
            elif field_key == 'HCOM_TIMEOUT':
                editor_color = FG_CYAN
                field_name = "Timeout"
                help_text = "seconds before disconnecting idle instance"
            elif field_key == 'HCOM_SUBAGENT_TIMEOUT':
                editor_color = FG_CYAN
                field_name = "Subagent Timeout"
                help_text = "seconds before disconnecting idle subagent"
            elif field_key == 'HCOM_TERMINAL':
                editor_color = FG_CYAN
                field_name = "Terminal"
                help_text = "launch in new window, current window, or custom terminal"
            elif field_key == 'HCOM_HINTS':
                editor_color = FG_CYAN
                field_name = "Hints"
                help_text = "text appended to all messages this instance receives"
            elif field_key == 'HCOM_TAG':
                editor_color = FG_CYAN
                field_name = "Tag"
                help_text = "identifier to create groups with @-mention"
            elif field_key.startswith('HCOM_'):
                # Other HCOM fields
                editor_color = FG_CYAN
                field_name = field_key.replace('HCOM_', '').replace('_', ' ').title()
                help_text = "hcom configuration variable"
            else:
                # Custom env vars
                editor_color = FG_CUSTOM_ENV
                field_name = field_key
                help_text = "custom environment variable"

            # Header line - bold field name, regular help text
            header = f"{editor_color}{BOLD}{field_name}:{RESET} {FG_GRAY}{help_text}{RESET}"
            lines.append(separator_line(width))
            lines.append(header)
            lines.append('')  # Blank line between header and input
            # Render editor with wrapping support
            editor_lines = render_text_input(field_value, cursor_pos, width, editor_content_rows, prefix="")
            lines.extend(editor_lines)
            # Separator after editor input
            lines.append(separator_line(width))
        else:
            # Separator at bottom when no editor
            lines.append(separator_line(width))

        return lines[:height]


    def handle_key(self, key: str):
        """Handle keys in Launch mode - with cursor-based bottom bar editing"""

        # UP/DOWN navigation (unchanged)
        if key == 'UP':
            if self.state.launch_field == LaunchField.CLAUDE_SECTION:
                if self.state.claude_cursor > -1:
                    self.state.claude_cursor -= 1
                else:
                    self.state.launch_field = LaunchField.LAUNCH_BTN
            elif self.state.launch_field == LaunchField.HCOM_SECTION:
                if self.state.hcom_cursor > -1:
                    self.state.hcom_cursor -= 1
                else:
                    self.state.launch_field = LaunchField.CLAUDE_SECTION
                    self.state.claude_cursor = -1
            elif self.state.launch_field == LaunchField.CUSTOM_ENV_SECTION:
                if self.state.custom_env_cursor > -1:
                    self.state.custom_env_cursor -= 1
                else:
                    self.state.launch_field = LaunchField.HCOM_SECTION
                    self.state.hcom_cursor = -1
            elif self.state.launch_field == LaunchField.OPEN_EDITOR:
                self.state.launch_field = LaunchField.CUSTOM_ENV_SECTION
                self.state.custom_env_cursor = -1
            else:
                fields = list(LaunchField)
                idx = fields.index(self.state.launch_field)
                self.state.launch_field = fields[(idx - 1) % len(fields)]

        elif key == 'DOWN':
            if self.state.launch_field == LaunchField.CLAUDE_SECTION:
                if self.state.claude_cursor == -1 and not self.state.claude_expanded:
                    self.state.launch_field = LaunchField.HCOM_SECTION
                    self.state.hcom_cursor = -1
                elif self.state.claude_expanded:
                    max_idx = len(self.build_claude_fields()) - 1
                    if self.state.claude_cursor < max_idx:
                        self.state.claude_cursor += 1
                    else:
                        self.state.launch_field = LaunchField.HCOM_SECTION
                        self.state.hcom_cursor = -1
            elif self.state.launch_field == LaunchField.HCOM_SECTION:
                if self.state.hcom_cursor == -1 and not self.state.hcom_expanded:
                    self.state.launch_field = LaunchField.CUSTOM_ENV_SECTION
                    self.state.custom_env_cursor = -1
                elif self.state.hcom_expanded:
                    max_idx = len(self.build_hcom_fields()) - 1
                    if self.state.hcom_cursor < max_idx:
                        self.state.hcom_cursor += 1
                    else:
                        self.state.launch_field = LaunchField.CUSTOM_ENV_SECTION
                        self.state.custom_env_cursor = -1
            elif self.state.launch_field == LaunchField.CUSTOM_ENV_SECTION:
                if self.state.custom_env_cursor == -1 and not self.state.custom_env_expanded:
                    self.state.launch_field = LaunchField.OPEN_EDITOR
                elif self.state.custom_env_expanded:
                    max_idx = len(self.build_custom_env_fields()) - 1
                    if self.state.custom_env_cursor < max_idx:
                        self.state.custom_env_cursor += 1
                    else:
                        self.state.launch_field = LaunchField.OPEN_EDITOR
            else:
                fields = list(LaunchField)
                idx = fields.index(self.state.launch_field)
                self.state.launch_field = fields[(idx + 1) % len(fields)]
                if self.state.launch_field == LaunchField.CLAUDE_SECTION:
                    self.state.claude_cursor = -1
                elif self.state.launch_field == LaunchField.HCOM_SECTION:
                    self.state.hcom_cursor = -1
                elif self.state.launch_field == LaunchField.CUSTOM_ENV_SECTION:
                    self.state.custom_env_cursor = -1

        # LEFT/RIGHT: adjust count, cycle for cycle fields, cursor movement for text fields
        elif key == 'LEFT' or key == 'RIGHT':
            # COUNT field: adjust by ±1
            if self.state.launch_field == LaunchField.COUNT:
                try:
                    current = int(self.state.launch_count) if self.state.launch_count else 1
                    if key == 'RIGHT':
                        current = min(999, current + 1)
                    else:  # LEFT
                        current = max(1, current - 1)
                    self.state.launch_count = str(current)
                except ValueError:
                    self.state.launch_count = "1"
            else:
                field_info = self.get_current_field_info()
                if field_info:
                    field_key, field_value, cursor_pos = field_info

                    # Get field object to check type
                    field_obj = None
                    if self.state.launch_field == LaunchField.HCOM_SECTION and self.state.hcom_cursor >= 0:
                        fields = self.build_hcom_fields()
                        if self.state.hcom_cursor < len(fields):
                            field_obj = fields[self.state.hcom_cursor]

                    # Check if it's a cycle field
                    if field_obj and field_obj.field_type == 'cycle':
                        # Cycle through options
                        options = field_obj.options or []
                        if options:
                            if field_value in options:
                                idx = options.index(field_value)
                                new_idx = (idx + 1) if key == 'RIGHT' else (idx - 1)
                                new_idx = new_idx % len(options)
                            else:
                                new_idx = 0
                            self.state.config_edit[field_key] = options[new_idx]
                            self.state.config_field_cursors[field_key] = len(options[new_idx])
                            self.tui.save_config_to_file()
                    else:
                        # Text field: move cursor
                        if key == 'LEFT':
                            new_cursor = text_input_move_left(cursor_pos)
                        else:
                            new_cursor = text_input_move_right(field_value, cursor_pos)

                        # Update cursor
                        if field_key == 'prompt':
                            self.state.launch_prompt_cursor = new_cursor
                        elif field_key == 'system_prompt':
                            self.state.launch_system_prompt_cursor = new_cursor
                        elif field_key == 'append_system_prompt':
                            self.state.launch_append_system_prompt_cursor = new_cursor
                        else:
                            self.state.config_field_cursors[field_key] = new_cursor

        # ENTER: expand/collapse, toggle, cycle, launch
        elif key == 'ENTER':
            if self.state.launch_field == LaunchField.CLAUDE_SECTION and self.state.claude_cursor == -1:
                self.state.claude_expanded = not self.state.claude_expanded
            elif self.state.launch_field == LaunchField.HCOM_SECTION and self.state.hcom_cursor == -1:
                self.state.hcom_expanded = not self.state.hcom_expanded
            elif self.state.launch_field == LaunchField.CUSTOM_ENV_SECTION and self.state.custom_env_cursor == -1:
                self.state.custom_env_expanded = not self.state.custom_env_expanded
            elif self.state.launch_field == LaunchField.CLAUDE_SECTION and self.state.claude_cursor >= 0:
                fields = self.build_claude_fields()
                if self.state.claude_cursor < len(fields):
                    field = fields[self.state.claude_cursor]
                    if field.field_type == 'checkbox' and field.key == 'background':
                        self.state.launch_background = not self.state.launch_background
                        self.tui.save_launch_state()
            elif self.state.launch_field == LaunchField.HCOM_SECTION and self.state.hcom_cursor >= 0:
                fields = self.build_hcom_fields()
                if self.state.hcom_cursor < len(fields):
                    field = fields[self.state.hcom_cursor]
                    if field.field_type == 'checkbox':
                        current = self.state.config_edit.get(field.key, '')
                        new_value = '0' if current == '1' else '1'
                        self.state.config_edit[field.key] = new_value
                        self.tui.save_config_to_file()
            elif self.state.launch_field == LaunchField.LAUNCH_BTN:
                self.do_launch()
            elif self.state.launch_field == LaunchField.OPEN_EDITOR:
                self.tui.open_config_in_editor()

        # BACKSPACE: delete char before cursor
        elif key == 'BACKSPACE':
            field_info = self.get_current_field_info()
            if field_info:
                field_key, field_value, cursor_pos = field_info
                new_value, new_cursor = text_input_backspace(field_value, cursor_pos)
                self.update_field(field_key, new_value, new_cursor)

        # ESC: clear field
        elif key == 'ESC':
            if self.state.launch_field == LaunchField.CLAUDE_SECTION:
                if self.state.claude_cursor >= 0:
                    fields = self.build_claude_fields()
                    if self.state.claude_cursor < len(fields):
                        field = fields[self.state.claude_cursor]
                        if field.key == 'prompt':
                            self.state.launch_prompt = ""
                            self.state.launch_prompt_cursor = 0
                            self.tui.save_launch_state()
                        elif field.key == 'system_prompt':
                            self.state.launch_system_prompt = ""
                            self.state.launch_system_prompt_cursor = 0
                            self.tui.save_launch_state()
                        elif field.key == 'append_system_prompt':
                            self.state.launch_append_system_prompt = ""
                            self.state.launch_append_system_prompt_cursor = 0
                            self.tui.save_launch_state()
                        elif field.key == 'claude_args':
                            self.state.config_edit['HCOM_CLAUDE_ARGS'] = ""
                            self.state.config_field_cursors['HCOM_CLAUDE_ARGS'] = 0
                            self.tui.save_config_to_file()
                            self.tui.load_launch_state()
                else:
                    self.state.claude_expanded = False
                    self.state.claude_cursor = -1
            elif self.state.launch_field == LaunchField.HCOM_SECTION:
                if self.state.hcom_cursor >= 0:
                    fields = self.build_hcom_fields()
                    if self.state.hcom_cursor < len(fields):
                        field = fields[self.state.hcom_cursor]
                        self.state.config_edit[field.key] = ""
                        self.state.config_field_cursors[field.key] = 0
                        self.tui.save_config_to_file()
                else:
                    self.state.hcom_expanded = False
                    self.state.hcom_cursor = -1
            elif self.state.launch_field == LaunchField.CUSTOM_ENV_SECTION:
                if self.state.custom_env_cursor >= 0:
                    fields = self.build_custom_env_fields()
                    if self.state.custom_env_cursor < len(fields):
                        field = fields[self.state.custom_env_cursor]
                        self.state.config_edit[field.key] = ""
                        self.state.config_field_cursors[field.key] = 0
                        self.tui.save_config_to_file()
                else:
                    self.state.custom_env_expanded = False
                    self.state.custom_env_cursor = -1
            elif self.state.launch_field == LaunchField.COUNT:
                self.state.launch_count = "1"

        # CTRL_R: Reset config to defaults (two-step confirmation)
        elif key == 'CTRL_R':
            is_confirming = self.state.pending_reset and (time.time() - self.state.pending_reset_time) <= self.tui.CONFIRMATION_TIMEOUT

            if is_confirming:
                # Execute config reset
                try:
                    result = reset_config()
                    if result == 0:
                        self.tui.load_config_from_file()
                        self.tui.load_launch_state()
                        self.tui.flash("Config reset to defaults")
                    else:
                        self.tui.flash_error("Failed to reset config")
                except Exception as e:
                    self.tui.flash_error(f"Reset failed: {str(e)}")
                finally:
                    self.state.pending_reset = False
            else:
                # Show confirmation (first press)
                self.state.pending_reset = True
                self.state.pending_reset_time = time.time()
                self.tui.flash(f"{FG_WHITE}Confirm backup + reset config to defaults? (Ctrl+R again){RESET}", duration=self.tui.CONFIRMATION_FLASH_DURATION, color='white')

        # SPACE and printable: insert at cursor
        elif key == 'SPACE' or (key and len(key) == 1 and key.isprintable()):
            char = ' ' if key == 'SPACE' else key
            field_info = self.get_current_field_info()
            if field_info:
                field_key, field_value, cursor_pos = field_info

                # Validate for special fields
                if field_key == 'HCOM_TAG':
                    override = CONFIG_FIELD_OVERRIDES.get(field_key, {})
                    allowed_pattern = override.get('allowed_chars')
                    if allowed_pattern:
                        test_value = field_value[:cursor_pos] + char + field_value[cursor_pos:]
                        if not re.match(allowed_pattern, test_value):
                            return

                new_value, new_cursor = text_input_insert(field_value, cursor_pos, char)
                self.update_field(field_key, new_value, new_cursor)


    def get_command_preview(self) -> str:
        """Build preview using spec (matches exactly what will be launched)"""
        try:
            # Load spec and update with form values (same logic as do_launch)
            claude_args_str = self.state.config_edit.get('HCOM_CLAUDE_ARGS', '')
            spec = resolve_claude_args(None, claude_args_str if claude_args_str else None)

            # Update spec with background and prompt
            spec = spec.update(
                background=self.state.launch_background,
                prompt=self.state.launch_prompt,
            )

            # Build tokens manually to support both system prompt types
            tokens = list(spec.clean_tokens)
            if self.state.launch_system_prompt:
                tokens.extend(['--system-prompt', self.state.launch_system_prompt])
            if self.state.launch_append_system_prompt:
                tokens.extend(['--append-system-prompt', self.state.launch_append_system_prompt])

            # Re-parse to get proper spec
            spec = resolve_claude_args(tokens, None)

            # Build preview
            parts = []

            # Environment variables (read from config_fields - source of truth)
            env_parts = []
            tag = self.state.config_edit.get('HCOM_TAG', '')
            if tag:
                tag_display = tag if len(tag) <= 15 else tag[:12] + "..."
                env_parts.append(f"HCOM_TAG={tag_display}")
            if env_parts:
                parts.append(" ".join(env_parts))

            # Base command
            count = self.state.launch_count if self.state.launch_count else "1"
            parts.append(f"hcom {count}")

            # Claude args from spec (truncate long values for preview)
            tokens = spec.rebuild_tokens(include_system=True)
            if tokens:
                preview_tokens = []
                for token in tokens:
                    if len(token) > 30:
                        preview_tokens.append(f'"{token[:27]}..."')
                    elif ' ' in token:
                        preview_tokens.append(f'"{token}"')
                    else:
                        preview_tokens.append(token)
                parts.append("claude " + " ".join(preview_tokens))

            return " ".join(parts)
        except Exception:
            return "(preview unavailable - check HCOM_CLAUDE_ARGS)"


    def get_current_field_info(self) -> tuple[str, str, int] | None:
        """Get (field_key, field_value, cursor_pos) for currently selected field, or None"""
        if self.state.launch_field == LaunchField.CLAUDE_SECTION and self.state.claude_cursor >= 0:
            fields = self.build_claude_fields()
            if self.state.claude_cursor < len(fields):
                field = fields[self.state.claude_cursor]
                if field.key == 'prompt':
                    # Default cursor to end if not set or invalid
                    if self.state.launch_prompt_cursor > len(self.state.launch_prompt):
                        self.state.launch_prompt_cursor = len(self.state.launch_prompt)
                    return ('prompt', self.state.launch_prompt, self.state.launch_prompt_cursor)
                elif field.key == 'system_prompt':
                    if self.state.launch_system_prompt_cursor > len(self.state.launch_system_prompt):
                        self.state.launch_system_prompt_cursor = len(self.state.launch_system_prompt)
                    return ('system_prompt', self.state.launch_system_prompt, self.state.launch_system_prompt_cursor)
                elif field.key == 'append_system_prompt':
                    if self.state.launch_append_system_prompt_cursor > len(self.state.launch_append_system_prompt):
                        self.state.launch_append_system_prompt_cursor = len(self.state.launch_append_system_prompt)
                    return ('append_system_prompt', self.state.launch_append_system_prompt, self.state.launch_append_system_prompt_cursor)
                elif field.key == 'claude_args':
                    value = self.state.config_edit.get('HCOM_CLAUDE_ARGS', '')
                    cursor = self.state.config_field_cursors.get('HCOM_CLAUDE_ARGS', len(value))
                    cursor = min(cursor, len(value))
                    self.state.config_field_cursors['HCOM_CLAUDE_ARGS'] = cursor
                    return ('HCOM_CLAUDE_ARGS', value, cursor)
        elif self.state.launch_field == LaunchField.HCOM_SECTION and self.state.hcom_cursor >= 0:
            fields = self.build_hcom_fields()
            if self.state.hcom_cursor < len(fields):
                field = fields[self.state.hcom_cursor]
                value = self.state.config_edit.get(field.key, '')
                cursor = self.state.config_field_cursors.get(field.key, len(value))
                cursor = min(cursor, len(value))
                self.state.config_field_cursors[field.key] = cursor
                return (field.key, value, cursor)
        elif self.state.launch_field == LaunchField.CUSTOM_ENV_SECTION and self.state.custom_env_cursor >= 0:
            fields = self.build_custom_env_fields()
            if self.state.custom_env_cursor < len(fields):
                field = fields[self.state.custom_env_cursor]
                value = self.state.config_edit.get(field.key, '')
                cursor = self.state.config_field_cursors.get(field.key, len(value))
                cursor = min(cursor, len(value))
                self.state.config_field_cursors[field.key] = cursor
                return (field.key, value, cursor)
        return None


    def update_field(self, field_key: str, new_value: str, new_cursor: int):
        """Update a launch field with new value and cursor position (extracted helper)"""
        if field_key == 'prompt':
            self.state.launch_prompt = new_value
            self.state.launch_prompt_cursor = new_cursor
            self.tui.save_launch_state()
        elif field_key == 'system_prompt':
            self.state.launch_system_prompt = new_value
            self.state.launch_system_prompt_cursor = new_cursor
            self.tui.save_launch_state()
        elif field_key == 'append_system_prompt':
            self.state.launch_append_system_prompt = new_value
            self.state.launch_append_system_prompt_cursor = new_cursor
            self.tui.save_launch_state()
        elif field_key == 'HCOM_CLAUDE_ARGS':
            self.state.config_edit[field_key] = new_value
            self.state.config_field_cursors[field_key] = new_cursor
            self.tui.save_config_to_file()
            self.tui.load_launch_state()
        else:
            self.state.config_edit[field_key] = new_value
            self.state.config_field_cursors[field_key] = new_cursor
            self.tui.save_config_to_file()


    def build_claude_fields(self) -> List[Field]:
        """Build Claude section fields from memory vars"""
        return [
            Field("prompt", "Prompt", "text", self.state.launch_prompt, hint="text string"),
            Field("system_prompt", "System Prompt", "text", self.state.launch_system_prompt, hint="text string"),
            Field("append_system_prompt", "Append System Prompt", "text", self.state.launch_append_system_prompt, hint="text string"),
            Field("background", "Headless", "checkbox", self.state.launch_background, hint="enter to toggle"),
            Field("claude_args", "Claude Args", "text", self.state.config_edit.get('HCOM_CLAUDE_ARGS', ''), hint="flags string"),
        ]


    def build_hcom_fields(self) -> List[Field]:
        """Build HCOM section fields - always show all expected HCOM vars"""
        # Extract expected keys from DEFAULT_CONFIG_DEFAULTS (excluding HCOM_CLAUDE_ARGS)
        expected_keys = [
            line.split('=')[0] for line in DEFAULT_CONFIG_DEFAULTS
            if line.startswith('HCOM_') and not line.startswith('HCOM_CLAUDE_ARGS=')
        ]

        fields = []
        for key in expected_keys:
            display_name = key.replace('HCOM_', '').replace('_', ' ').title()
            override = CONFIG_FIELD_OVERRIDES.get(key, {})
            field_type = override.get('type', 'text')
            options = override.get('options')
            if callable(options):
                options = options()
            hint = override.get('hint', '')
            value = self.state.config_edit.get(key, '')
            fields.append(Field(key, display_name, field_type, value, options if isinstance(options, list) or options is None else None, hint))

        # Also include any extra HCOM_* vars from config_fields (user-added)
        for key in sorted(self.state.config_edit.keys()):
            if key.startswith('HCOM_') and key != 'HCOM_CLAUDE_ARGS' and key not in expected_keys:
                display_name = key.replace('HCOM_', '').replace('_', ' ').title()
                override = CONFIG_FIELD_OVERRIDES.get(key, {})
                field_type = override.get('type', 'text')
                options = override.get('options')
                if callable(options):
                    options = options()
                hint = override.get('hint', '')
                fields.append(Field(key, display_name, field_type, self.state.config_edit.get(key, ''), options if isinstance(options, list) or options is None else None, hint))

        return fields


    def build_custom_env_fields(self) -> List[Field]:
        """Build Custom Env section fields from config_fields"""
        return [Field(key, key, 'text', self.state.config_edit.get(key, ''))
                for key in sorted(self.state.config_edit.keys())
                if not key.startswith('HCOM_')]


    def render_section_fields(
        self,
        lines: List[str],
        fields: List[Field],
        expanded: bool,
        section_field: LaunchField,
        section_cursor: int,
        width: int,
        color: str
    ) -> int | None:
        """Render fields for an expandable section (extracted helper)

        Returns selected_field_start_line if a field is selected, None otherwise.
        """
        selected_field_start_line = None

        if expanded or (self.state.launch_field == section_field and section_cursor >= 0):
            visible_fields = fields if expanded else fields[:3]
            for i, field in enumerate(visible_fields):
                field_selected = (self.state.launch_field == section_field and section_cursor == i)
                if field_selected:
                    selected_field_start_line = len(lines)
                lines.append(self.render_field(field, field_selected, width, color))
            if not expanded and len(fields) > 3:
                lines.append(f"{FG_GRAY}    +{len(fields) - 3} more (enter to expand){RESET}")

        return selected_field_start_line


    def render_field(self, field: Field, selected: bool, width: int, value_color: str | None = None) -> str:
        """Render a single field line"""
        indent = "    "
        # Default to standard orange if not specified
        if value_color is None:
            value_color = FG_ORANGE

        # Determine if field is in config (for proper state display)
        in_config = field.key in self.state.config_edit

        # Format value based on type
        # For Claude fields, use cached defaults from HCOM_CLAUDE_ARGS
        if field.key in ('prompt', 'system_prompt', 'append_system_prompt', 'background'):
            default_prompt, default_system, default_append, default_background = self._get_claude_defaults()
            default = {
                'prompt': default_prompt,
                'system_prompt': default_system,
                'append_system_prompt': default_append,
                'background': default_background,
            }[field.key]
        else:
            default = CONFIG_DEFAULTS.get(field.key, '')

        # Check if field has validation error
        has_error = field.key in self.state.validation_errors

        if field.field_type == 'checkbox':
            # Handle both boolean (Claude section) and string '1'/'0' (HCOM section)
            is_checked = field.value is True or field.value == '1'
            check = '●' if is_checked else '○'
            # Color if differs from default
            default_checked = default is True or default == '1'
            is_modified = is_checked != default_checked
            value_str = f"{value_color if is_modified else FG_WHITE}{check}{RESET}"
        elif field.field_type == 'text':
            if field.value:
                # Has value - color only if different from default (normalize quotes and whitespace)
                field_value_normalized = str(field.value).strip().strip("'\"").strip()
                default_normalized = str(default).strip().strip("'\"").strip()
                is_modified = field_value_normalized != default_normalized
                color = value_color if is_modified else FG_WHITE
                value_str = f"{color}{field.value}{RESET}"
            else:
                # Empty - check what runtime will actually use
                field_value_normalized = str(field.value).strip().strip("'\"").strip()
                default_normalized = str(default).strip().strip("'\"").strip()
                # Runtime uses empty if field doesn't auto-revert to default
                # For HCOM_CLAUDE_ARGS and Prompt, empty stays empty (doesn't use default)
                runtime_reverts_to_default = field.key not in ('HCOM_CLAUDE_ARGS', 'prompt')

                if runtime_reverts_to_default:
                    # Empty → runtime uses default → NOT modified
                    value_str = f"{FG_WHITE}(default: {default}){RESET}" if default else f"{FG_WHITE}(empty){RESET}"
                else:
                    # Empty → runtime uses "" → IS modified if default is non-empty
                    is_modified = bool(default_normalized)  # Modified if default exists
                    if is_modified:
                        # Colored with default hint (no RESET between to preserve background when selected)
                        value_str = f"{value_color}(empty) {FG_GRAY}default: {default}{RESET}"
                    else:
                        # Empty and no default
                        value_str = f"{FG_WHITE}(empty){RESET}"
        else:  # cycle, numeric
            if field.value:
                # Has value - color only if different from default (normalize quotes)
                field_value_normalized = str(field.value).strip().strip("'\"")
                default_normalized = str(default).strip().strip("'\"")
                is_modified = field_value_normalized != default_normalized
                color = value_color if is_modified else FG_WHITE
                value_str = f"{color}{field.value}{RESET}"
            else:
                # Empty - check what runtime will actually use
                if field.field_type == 'numeric':
                    # Timeout fields: empty → runtime uses default → NOT modified
                    value_str = f"{FG_WHITE}(default: {default}){RESET}" if default else f"{FG_WHITE}(empty){RESET}"
                else:
                    # Cycle fields: empty → runtime uses default → NOT modified
                    value_str = f"{FG_WHITE}(default: {default}){RESET}" if default else f"{FG_WHITE}(empty){RESET}"

        if field.hint and selected:
            value_str += f"{BG_CHARCOAL}  {FG_GRAY}• {field.hint}{RESET}"

        # Build line
        if selected:
            arrow_color = FG_RED if has_error else FG_WHITE
            line = f"{indent}{BG_CHARCOAL}{arrow_color}{BOLD}▸ {field.display_name}:{RESET}{BG_CHARCOAL} {value_str}"
            return bg_ljust(truncate_ansi(line, width), width, BG_CHARCOAL)
        else:
            return truncate_ansi(f"{indent}{FG_WHITE}{field.display_name}:{RESET} {value_str}", width)


    def get_footer(self) -> str:
        """Return context-sensitive footer for Launch screen"""
        # Count field
        if self.state.launch_field == LaunchField.COUNT:
            return f"{FG_GRAY}tab: switch  ←→: adjust  esc: reset to 1  ctrl+r: reset config{RESET}"

        # Launch button
        elif self.state.launch_field == LaunchField.LAUNCH_BTN:
            return f"{FG_GRAY}tab: switch  enter: launch  ctrl+r: reset config{RESET}"
        elif self.state.launch_field == LaunchField.OPEN_EDITOR:
            cmd, label = self.tui.resolve_editor_command()
            if cmd:
                friendly = label or 'VS Code'
                return f"{FG_GRAY}tab: switch  enter: open {friendly}{RESET}"
            return f"{FG_GRAY}tab: switch  enter: install code CLI or set $EDITOR{RESET}"

        # Section headers (cursor == -1)
        elif self.state.launch_field == LaunchField.CLAUDE_SECTION and self.state.claude_cursor == -1:
            return f"{FG_GRAY}tab: switch  enter: expand/collapse  ctrl+r: reset config{RESET}"
        elif self.state.launch_field == LaunchField.HCOM_SECTION and self.state.hcom_cursor == -1:
            return f"{FG_GRAY}tab: switch  enter: expand/collapse  ctrl+r: reset config{RESET}"
        elif self.state.launch_field == LaunchField.CUSTOM_ENV_SECTION and self.state.custom_env_cursor == -1:
            return f"{FG_GRAY}tab: switch  enter: expand/collapse  ctrl+r: reset config{RESET}"

        # Fields within sections (cursor >= 0)
        elif self.state.launch_field == LaunchField.CLAUDE_SECTION and self.state.claude_cursor >= 0:
            fields = self.build_claude_fields()
            if self.state.claude_cursor < len(fields):
                field = fields[self.state.claude_cursor]
                if field.field_type == 'checkbox':
                    return f"{FG_GRAY}tab: switch  enter: toggle  ctrl+r: reset config{RESET}"
                else:  # text fields
                    return f"{FG_GRAY}tab: switch  type: edit  ←→: cursor  esc: clear  ctrl+r: reset config{RESET}"

        elif self.state.launch_field == LaunchField.HCOM_SECTION and self.state.hcom_cursor >= 0:
            fields = self.build_hcom_fields()
            if self.state.hcom_cursor < len(fields):
                field = fields[self.state.hcom_cursor]
                if field.field_type == 'checkbox':
                    return f"{FG_GRAY}tab: switch  enter: toggle  ctrl+r: reset config{RESET}"
                elif field.field_type == 'cycle':
                    return f"{FG_GRAY}tab: switch  ←→: cycle options  esc: clear  ctrl+r: reset config{RESET}"
                elif field.field_type == 'numeric':
                    return f"{FG_GRAY}tab: switch  type: digits  ←→: cursor  esc: clear  ctrl+r: reset config{RESET}"
                else:  # text fields
                    return f"{FG_GRAY}tab: switch  type: edit  ←→: cursor  esc: clear  ctrl+r: reset config{RESET}"

        elif self.state.launch_field == LaunchField.CUSTOM_ENV_SECTION and self.state.custom_env_cursor >= 0:
            return f"{FG_GRAY}tab: switch  type: edit  ←→: cursor  esc: clear  ctrl+r: reset config{RESET}"

        # Fallback (should not happen)
        return f"{FG_GRAY}tab: switch  ctrl+r: reset config{RESET}"


    def do_launch(self):
        """Execute launch using full spec integration"""
        # Check for validation errors first
        if self.state.validation_errors:
            error_fields = ', '.join(self.state.validation_errors.keys())
            self.tui.flash_error(f"Fix config errors before launching: {error_fields}", duration=15.0)
            return

        # Parse count
        try:
            count = int(self.state.launch_count) if self.state.launch_count else 1
        except ValueError:
            self.tui.flash_error("Invalid count - must be number")
            return

        # Load current spec from config
        try:
            claude_args_str = self.state.config_edit.get('HCOM_CLAUDE_ARGS', '')
            spec = resolve_claude_args(None, claude_args_str if claude_args_str else None)
        except Exception as e:
            self.tui.flash_error(f"Failed to parse HCOM_CLAUDE_ARGS: {e}")
            return

        # Check for parse errors BEFORE update (update loses original errors)
        if spec.errors:
            self.tui.flash_error(f"Invalid HCOM_CLAUDE_ARGS: {'; '.join(spec.errors)}")
            return

        # Update spec with background and prompt
        spec = spec.update(
            background=self.state.launch_background,
            prompt=self.state.launch_prompt,  # Always pass value (empty string deletes)
        )

        # Build tokens manually to support both system prompt types
        tokens = list(spec.clean_tokens)
        if self.state.launch_system_prompt:
            tokens.extend(['--system-prompt', self.state.launch_system_prompt])
        if self.state.launch_append_system_prompt:
            tokens.extend(['--append-system-prompt', self.state.launch_append_system_prompt])

        # Re-parse to get proper spec
        spec = resolve_claude_args(tokens, None)

        # Build argv using spec (preserves all flags from HCOM_CLAUDE_ARGS)
        argv = [str(count), 'claude'] + spec.rebuild_tokens(include_system=True)

        # Set env vars if specified (read from config_fields - source of truth)
        env_backup = {}
        try:
            tag = self.state.config_edit.get('HCOM_TAG', '')
            if tag:
                env_backup['HCOM_TAG'] = os.environ.get('HCOM_TAG')
                os.environ['HCOM_TAG'] = tag

            # Show launching message
            self.tui.flash(f"Launching {count} instances...")
            self.tui.render()  # Force update to show message

            # Call hcom.cmd_launch (handles all validation)
            # Add --no-auto-watch flag to prevent opening another watch window
            reload_config()
            # Close stale DB connection before launch - ensures fresh max event ID
            # (fixes inode reuse issue on macOS where TUI's connection persists after reset)
            from ..core.db import close_db
            close_db()
            result = cmd_launch(argv + ['--no-auto-watch'])

            if result == 0:  # Success
                # Switch to Manage screen to see new instances
                self.tui.mode = Mode.MANAGE
                self.tui.flash(f"Launched {count} instances")
                self.tui.load_status()  # Refresh immediately
            else:
                self.tui.flash_error("Launch failed - check instances")

        except Exception as e:
            # cmd_launch raises CLIError for validation failures
            self.tui.flash_error(str(e))
        finally:
            # Restore env (clean up)
            for key, val in env_backup.items():
                if val is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = val