"""Launch mode screen implementation.

This module implements the LaunchScreen class for creating new instances.
It provides a form-based interface with expandable sections for configuration.

Screen Layout
-------------
    ┌─────────────────────────────────────────┐
    │   Tool: claude                          │
    │   Count: 1                              │
    │   ▶ Launch ⏎                            │
    │ ─────────────────────────────────────── │
    │   ▼ Claude • 2/5                        │
    │       Prompt: [text input]              │
    │       System Prompt: [text input]       │
    │       Headless: [ ]                     │
    │   ▶ HCOM • 0/4                          │
    │   ▶ Custom Env • 0/3                    │
    │ ─────────────────────────────────────── │
    │   ↗ Open config in VS Code              │
    │ ═══════════════════════════════════════ │
    │   Editor (when text field selected)     │
    └─────────────────────────────────────────┘

Key Bindings
------------
- UP/DOWN: Navigate between fields
- LEFT/RIGHT: Cycle through options (tool, numeric, multi_cycle)
- ENTER: Launch instances (on button), expand/collapse sections
- TAB: Switch to Manage mode
- ESC: Collapse sections, clear text input

Field Types
-----------
- "text": Free-form text input, opens editor when selected
- "checkbox": Toggle on/off with Enter or Space
- "cycle": LEFT/RIGHT cycles through fixed options
- "numeric": LEFT/RIGHT increment/decrement
- "multi_cycle": Cycle + Space to add multiple values

Sections
--------
- Tool Section (Claude/Gemini/Codex): Tool-specific options
- HCOM Section: Common HCOM config (tag, hints, timeout, terminal)
- Custom Env Section: User-defined environment variables

Storage
-------
Field values are stored in different places:
- LaunchState attributes: prompt, system_prompt, etc.
- config_edit dict: HCOM_* config values
- Both are persisted to config.env on mode switch or exit
"""

from __future__ import annotations
from typing import List, TYPE_CHECKING, Any, cast, Dict
import os
import re
import shlex
import time

if TYPE_CHECKING:
    from ..ui import HcomTUI, UIState

# Import types
from ..ui import Field, LaunchField, Mode

# Import rendering utilities
from ..ui import (
    bg_ljust,
    truncate_ansi,
    separator_line,
)

# Import input utilities
from ..ui import (
    render_text_input,
    calculate_text_input_rows,
    text_input_insert,
    text_input_backspace,
    text_input_move_left,
    text_input_move_right,
)

# Import ANSI codes directly to avoid circular import
from .colors import (
    RESET,
    BOLD,
    DIM,
    FG_WHITE,
    FG_BLACK,
    FG_GRAY,
    FG_ORANGE,
    FG_CYAN,
    FG_RED,
    FG_GREEN,
    BG_CHARCOAL,
    BG_ORANGE,
    BOX_H,
)

# Import non-color constants from shared
from ..shared import (
    DEFAULT_CONFIG_DEFAULTS,
    RELEASED_TOOLS,
    RELEASED_BACKGROUND,
    IS_WINDOWS,
)
from ..ui import (
    CONFIG_DEFAULTS,
    CONFIG_FIELD_OVERRIDES,
    FG_CLAUDE_ORANGE,
    FG_CUSTOM_ENV,
)
from ..core.config import reload_config
from ..shared import resolve_claude_args
from ..tools.codex.args import resolve_codex_args
from ..tools.gemini.args import resolve_gemini_args
from ..launcher import launch as unified_launch
from ..commands.admin import reset_config

# PTY-only tools that don't work on Windows
_PTY_ONLY_TOOLS = {"gemini", "codex"}

# Field storage registry - maps field_key to storage configuration
# Format: (value_attr, cursor_attr, save_action)
#   value_attr: state attribute name, or None to use config_edit[field_key]
#   cursor_attr: state attribute name, or None to use config_field_cursors[field_key]
#   save_action: "launch" | "config" | "config_reload"
# Special value "tool_specific" means the prompt field varies by current tool
_FIELD_STORAGE = {
    # Tool-specific prompt (resolved dynamically)
    "prompt": "tool_specific",
    # Fixed launch state fields (attr names on LaunchState)
    "system_prompt": ("system_prompt", "system_prompt_cursor", "launch"),
    "append_system_prompt": ("append_system_prompt", "append_system_prompt_cursor", "launch"),
    "allowed_tools": ("allowed_tools", None, "launch"),  # multi_cycle field, no cursor
    "codex_system_prompt": ("codex_system_prompt", "codex_system_prompt_cursor", "launch"),
    "gemini_system_prompt": ("gemini_system_prompt", "gemini_system_prompt_cursor", "launch"),
    # Config fields that trigger reload after save
    "HCOM_CLAUDE_ARGS": (None, None, "config_reload"),
    "HCOM_CODEX_ARGS": (None, None, "config_reload"),
    "HCOM_GEMINI_ARGS": (None, None, "config_reload"),
}

# Tool-specific prompt storage mapping (attr names on LaunchState)
_PROMPT_STORAGE = {
    "claude": ("prompt", "prompt_cursor"),
    "gemini": ("gemini_prompt", "gemini_prompt_cursor"),
    "codex": ("codex_prompt", "codex_prompt_cursor"),
}


class LaunchScreen:
    """Launch mode screen: form-based instance creation.

    Provides a configurable form for launching new HCOM instances.
    Features expandable sections for tool-specific, HCOM, and custom
    environment configuration.

    The screen uses a cursor-based navigation model:
    - Top-level fields: Tool, Count, Launch button
    - Expandable sections: Claude/Gemini/Codex, HCOM, Custom Env
    - Each section has a header and fields that can be navigated

    Attributes:
        state: Reference to shared UIState object.
        tui: Reference to parent HcomTUI for flash notifications.

    Class Attributes:
        _claude_defaults_cache: Cached defaults from HCOM_CLAUDE_ARGS.
        _tool_options: Available tool types (filtered by platform).
    """

    _claude_defaults_cache = None  # Class-level cache for claude defaults
    # Filter out PTY-only tools on Windows (they require Unix-only APIs)
    _tool_options = tuple(
        t for t in ("claude", "gemini", "codex") if t in RELEASED_TOOLS and not (IS_WINDOWS and t in _PTY_ONLY_TOOLS)
    )

    def __init__(self, state: UIState, tui: HcomTUI):
        """Initialize LaunchScreen.

        Args:
            state: Shared UIState object containing LaunchState.
            tui: Parent HcomTUI for flash notifications and config operations.
        """
        self.state = state  # Shared state (explicit dependency)
        self.tui = tui  # For commands only (flash, config loading, cmd_launch)

    @staticmethod
    def _is_tool_installed(tool: str) -> bool:
        """Check if tool CLI is installed and available in PATH.

        Args:
            tool: Tool name ("claude", "gemini", or "codex").

        Returns:
            True if the tool is installed and executable.
        """
        from ..core.tool_utils import is_tool_installed

        return is_tool_installed(tool)

    def _get_claude_defaults(self) -> tuple[str, str, str, bool]:
        """Get default values from HCOM_CLAUDE_ARGS config.

        Parses HCOM_CLAUDE_ARGS to extract default values for prompt,
        system prompt, append system prompt, and background mode.
        Results are cached at class level to avoid repeated parsing.

        Returns:
            Tuple of (prompt, system_prompt, append_system_prompt, is_background).
        """
        if LaunchScreen._claude_defaults_cache is None:
            claude_args_default = CONFIG_DEFAULTS.get("HCOM_CLAUDE_ARGS", "")
            spec = resolve_claude_args(None, claude_args_default if claude_args_default else None)
            LaunchScreen._claude_defaults_cache = (
                spec.positional_tokens[0] if spec.positional_tokens else "",
                spec.get_flag_value("--system-prompt") or "",
                spec.get_flag_value("--append-system-prompt") or "",
                spec.is_background,
            )
        return LaunchScreen._claude_defaults_cache

    @classmethod
    def invalidate_defaults_cache(cls):
        """Clear cached defaults (call after config reload)."""
        cls._claude_defaults_cache = None

    def build(self, height: int, width: int) -> List[str]:
        """Build the complete launch screen as a list of lines.

        Constructs the visual layout including:
        - Tool selector and count fields
        - Launch button
        - Expandable config sections (Tool, HCOM, Custom Env)
        - Text editor (when a text field is selected)

        The screen auto-scrolls to keep the selected field visible.

        Args:
            height: Available height in terminal rows.
            width: Available width in terminal columns.

        Returns:
            List of formatted line strings ready for display.
        """
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

        lines.append("")  # Top padding

        # Tool field
        tool_selected = self.state.launch.current_field == LaunchField.TOOL
        tool_installed = self._is_tool_installed(self.state.launch.tool)
        install_indicator = f"{FG_GREEN}✓{RESET}" if tool_installed else f"{FG_RED}✗{RESET}"
        if tool_selected:
            selected_field_start_line = len(lines)
            line = (
                f"  {BG_CHARCOAL}{FG_WHITE}{BOLD}\u25b8 Tool:{RESET}{BG_CHARCOAL} "
                f"{FG_ORANGE}{self.state.launch.tool}{RESET}{BG_CHARCOAL} {install_indicator}{BG_CHARCOAL}  {FG_GRAY}\u2022 \u2190\u2192 cycle{RESET}"
            )
            lines.append(bg_ljust(line, width, BG_CHARCOAL))
        else:
            lines.append(f"  {FG_WHITE}Tool:{RESET} {FG_ORANGE}{self.state.launch.tool}{RESET} {install_indicator}")

        # Count field (with left padding)
        count_selected = self.state.launch.current_field == LaunchField.COUNT
        if count_selected:
            selected_field_start_line = len(lines)
            line = f"  {BG_CHARCOAL}{FG_WHITE}{BOLD}\u25b8 Count:{RESET}{BG_CHARCOAL} {FG_ORANGE}{self.state.launch.count}{RESET}{BG_CHARCOAL}  {FG_GRAY}\u2022 \u2190\u2192 adjust{RESET}"
            lines.append(bg_ljust(line, width, BG_CHARCOAL))
        else:
            lines.append(f"  {FG_WHITE}Count:{RESET} {FG_ORANGE}{self.state.launch.count}{RESET}")

        # Launch button (with left padding)
        launch_selected = self.state.launch.current_field == LaunchField.LAUNCH_BTN
        if launch_selected:
            selected_field_start_line = len(lines)
            lines.append(f"  {BG_ORANGE}{FG_BLACK}{BOLD} \u25b6 Launch \u23ce {RESET}")
            # Show cwd when launch button is selected
            cwd = os.getcwd()
            max_cwd_width = width - 10  # Leave margin
            if len(cwd) > max_cwd_width:
                cwd = "\u2026" + cwd[-(max_cwd_width - 1) :]
            lines.append(f"  {BG_CHARCOAL}{FG_GRAY} \u2022 {FG_WHITE}{cwd} {RESET}")
        else:
            lines.append(f"  {FG_GRAY}\u25b6{RESET} {FG_ORANGE}{BOLD}Launch{RESET}")

        lines.append("")  # Spacer
        lines.append(f"{DIM}{FG_GRAY}{BOX_H * width}{RESET}")  # Separator (dim)
        lines.append("")  # Spacer

        # Tool section header (Claude/Gemini/Codex - with left padding)
        tool = self.state.launch.tool
        tool_title = tool.capitalize()  # claude -> Claude, gemini -> Gemini, codex -> Codex
        claude_selected = (
            self.state.launch.current_field == LaunchField.CLAUDE_SECTION and self.state.launch.claude_cursor == -1
        )
        expand_marker = "\u25bc" if self.state.launch.claude_expanded else "\u25b6"
        claude_fields = self.build_claude_fields()
        # Count fields modified from defaults
        claude_set = 0
        if tool == "claude":
            default_prompt, default_system, default_append, default_background = self._get_claude_defaults()
            if self.state.launch.background != default_background:
                claude_set += 1
            if self.state.launch.prompt != default_prompt:
                claude_set += 1
            if self.state.launch.system_prompt != default_system:
                claude_set += 1
            if self.state.launch.append_system_prompt != default_append:
                claude_set += 1
            # claude_args: check if raw value differs from default (normalize quotes)
            claude_args_val = self.state.config_edit.get("HCOM_CLAUDE_ARGS", "").strip().strip("'\"")
            claude_args_default_normalized = CONFIG_DEFAULTS.get("HCOM_CLAUDE_ARGS", "").strip().strip("'\"")
            if claude_args_val != claude_args_default_normalized:
                claude_set += 1
        else:
            # Gemini/Codex: count non-empty fields
            tool_prompt = self.state.launch.gemini_prompt if tool == "gemini" else self.state.launch.codex_prompt
            if tool_prompt:
                claude_set += 1
            # Count tool-specific system prompt
            if tool == "gemini" and self.state.launch.gemini_system_prompt:
                claude_set += 1
            elif tool == "codex" and self.state.launch.codex_system_prompt:
                claude_set += 1
            if tool in RELEASED_BACKGROUND and self.state.launch.background:
                claude_set += 1
        claude_total = len(claude_fields)
        claude_count = f" \u2022 {claude_set}/{claude_total}"
        if claude_selected:
            selected_field_start_line = len(lines)
            claude_action = "\u2190 collapse" if self.state.launch.claude_expanded else "\u2192 expand"
            claude_hint = f"{claude_count} \u2022 {claude_action}"
            line = f"  {BG_CHARCOAL}{FG_CLAUDE_ORANGE}{BOLD}{expand_marker} {tool_title}{RESET}{BG_CHARCOAL}  {FG_GRAY}{claude_hint}{RESET}"
            lines.append(bg_ljust(line, width, BG_CHARCOAL))
        else:
            lines.append(f"  {FG_CLAUDE_ORANGE}{BOLD}{expand_marker} {tool_title}{RESET}{FG_GRAY}{claude_count}{RESET}")

        # Preview modified fields when collapsed, or show description if none
        if not self.state.launch.claude_expanded:
            previews = []
            has_ui_prompt = False

            if tool == "claude":
                if self.state.launch.background != default_background:
                    previews.append("headless: true" if self.state.launch.background else "headless: false")
                if self.state.launch.prompt != default_prompt:
                    prompt_str = str(self.state.launch.prompt) if self.state.launch.prompt else ""
                    if prompt_str:
                        has_ui_prompt = True
                        prompt_preview = prompt_str[:20] + "..." if len(prompt_str) > 20 else prompt_str
                        previews.append(f'prompt: "{prompt_preview}"')
                if self.state.launch.system_prompt != default_system:
                    sys_str = str(self.state.launch.system_prompt) if self.state.launch.system_prompt else ""
                    sys_preview = sys_str[:20] + "..." if len(sys_str) > 20 else sys_str
                    previews.append(f'system: "{sys_preview}"')
                if self.state.launch.append_system_prompt != default_append:
                    append_str = (
                        str(self.state.launch.append_system_prompt) if self.state.launch.append_system_prompt else ""
                    )
                    append_preview = append_str[:20] + "..." if len(append_str) > 20 else append_str
                    previews.append(f'append: "{append_preview}"')
                if claude_args_val != claude_args_default_normalized:
                    args_str = str(claude_args_val) if claude_args_val else ""
                    args_preview = args_str[:25] + "..." if len(args_str) > 25 else args_str
                    previews.append(f'args: "{args_preview}"')
                if self.state.launch.allowed_tools:
                    tools_preview = self.state.launch.allowed_tools
                    if len(tools_preview) > 25:
                        tools_preview = tools_preview[:25] + "..."
                    previews.append(f"tools: {tools_preview}")
                # Check for interactive mode (no prompt)
                if not has_ui_prompt and not self.state.launch.prompt:
                    claude_args_str = self.state.config_edit.get("HCOM_CLAUDE_ARGS", "")
                    claude_spec = resolve_claude_args(None, claude_args_str if claude_args_str else None)
                    if not claude_spec.positional_tokens:
                        previews.append("no prompt")
            else:
                # Gemini/Codex previews
                tool_prompt = self.state.launch.gemini_prompt if tool == "gemini" else self.state.launch.codex_prompt
                if tool_prompt:
                    has_ui_prompt = True
                    prompt_preview = tool_prompt[:20] + "..." if len(tool_prompt) > 20 else tool_prompt
                    previews.append(f'prompt: "{prompt_preview}"')
                # Tool-specific system prompt preview
                sys_prompt = (
                    self.state.launch.gemini_system_prompt
                    if tool == "gemini"
                    else self.state.launch.codex_system_prompt
                )
                if sys_prompt:
                    sys_preview = sys_prompt[:20] + "..." if len(sys_prompt) > 20 else sys_prompt
                    previews.append(f'system: "{sys_preview}"')
                if tool in RELEASED_BACKGROUND and self.state.launch.background:
                    previews.append("headless: true")
                # Check for interactive mode (no prompt)
                if not has_ui_prompt:
                    args_key = f"HCOM_{tool.upper()}_ARGS"
                    args_str = self.state.config_edit.get(args_key, "")
                    has_args_prompt = False
                    if tool == "gemini":
                        gemini_spec = resolve_gemini_args(None, args_str if args_str else None)
                        has_args_prompt = bool(gemini_spec.positional_tokens) or gemini_spec.has_flag(
                            ["-i", "--prompt-interactive"]
                        )
                    elif tool == "codex":
                        codex_spec = resolve_codex_args(None, args_str if args_str else None)
                        has_args_prompt = bool(codex_spec.positional_tokens)
                    if not has_args_prompt:
                        previews.append("no prompt")

            if previews:
                preview_text = ", ".join(previews)
                lines.append(f"    {DIM}{FG_GRAY}{truncate_ansi(preview_text, width - 4)}{RESET}")
            else:
                # Show available fields description
                if tool == "claude":
                    lines.append(f"    {DIM}{FG_GRAY}prompt, system, append, tools, headless, args{RESET}")
                elif tool in RELEASED_BACKGROUND:
                    lines.append(f"    {DIM}{FG_GRAY}prompt, system, headless, args{RESET}")
                else:
                    lines.append(f"    {DIM}{FG_GRAY}prompt, system, args{RESET}")

        # Claude fields (if expanded or cursor inside)
        result = self.render_section_fields(
            lines,
            claude_fields,
            self.state.launch.claude_expanded,
            LaunchField.CLAUDE_SECTION,
            self.state.launch.claude_cursor,
            width,
            FG_CLAUDE_ORANGE,
        )
        if result is not None:
            selected_field_start_line = result

        # Add spacing after expanded section
        if self.state.launch.claude_expanded:
            lines.append("")

        # HCOM section header (with left padding)
        hcom_selected = (
            self.state.launch.current_field == LaunchField.HCOM_SECTION and self.state.launch.hcom_cursor == -1
        )
        expand_marker = "\u25bc" if self.state.launch.hcom_expanded else "\u25b6"
        hcom_fields = self.build_hcom_fields()

        # Count fields modified from defaults (considering runtime behavior)
        def is_field_modified(f):
            default = CONFIG_DEFAULTS.get(f.key, "")
            if not f.value:  # Empty
                # Fields where empty reverts to default at runtime
                if f.key in (
                    "HCOM_TERMINAL",
                    "HCOM_HINTS",
                    "HCOM_TAG",
                    "HCOM_SUBAGENT_TIMEOUT",
                ):
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
            hcom_action = "\u2190 collapse" if self.state.launch.hcom_expanded else "\u2192 expand"
            hcom_hint = f"{hcom_count} \u2022 {hcom_action}"
            line = (
                f"  {BG_CHARCOAL}{FG_CYAN}{BOLD}{expand_marker} HCOM{RESET}{BG_CHARCOAL}  {FG_GRAY}{hcom_hint}{RESET}"
            )
            lines.append(bg_ljust(line, width, BG_CHARCOAL))
        else:
            lines.append(f"  {FG_CYAN}{BOLD}{expand_marker} HCOM{RESET}{FG_GRAY}{hcom_count}{RESET}")

        # Preview modified fields when collapsed, or show description if none
        if not self.state.launch.hcom_expanded:
            if hcom_set > 0:
                previews = []
                for field in hcom_fields:
                    if is_field_modified(field):
                        val = field.value or ""
                        if field.field_type == "checkbox":
                            val_str = "true" if val == "true" else "false"
                        else:
                            val = str(val) if val else ""
                            val_str = val[:15] + "..." if len(val) > 15 else val
                        # Shorten field names
                        short_name = field.display_name.lower().replace("hcom ", "")
                        previews.append(f"{short_name}: {val_str}")
                if previews:
                    preview_text = ", ".join(previews)
                    lines.append(f"    {DIM}{FG_GRAY}{truncate_ansi(preview_text, width - 4)}{RESET}")
            else:
                lines.append(f"    {DIM}{FG_GRAY}tag, hints, timeout, terminal{RESET}")

        # HCOM fields
        result = self.render_section_fields(
            lines,
            hcom_fields,
            self.state.launch.hcom_expanded,
            LaunchField.HCOM_SECTION,
            self.state.launch.hcom_cursor,
            width,
            FG_CYAN,
        )
        if result is not None:
            selected_field_start_line = result

        # Add spacing after expanded section
        if self.state.launch.hcom_expanded:
            lines.append("")

        # Custom Env section header (with left padding)
        custom_selected = (
            self.state.launch.current_field == LaunchField.CUSTOM_ENV_SECTION
            and self.state.launch.custom_env_cursor == -1
        )
        expand_marker = "\u25bc" if self.state.launch.custom_env_expanded else "\u25b6"
        custom_fields = self.build_custom_env_fields()
        custom_set = sum(1 for f in custom_fields if f.value)
        custom_total = len(custom_fields)
        custom_count = f" \u2022 {custom_set}/{custom_total}"
        if custom_selected:
            selected_field_start_line = len(lines)
            custom_action = "\u2190 collapse" if self.state.launch.custom_env_expanded else "\u2192 expand"
            custom_hint = f"{custom_count} \u2022 {custom_action}"
            line = f"  {BG_CHARCOAL}{FG_CUSTOM_ENV}{BOLD}{expand_marker} Custom Env{RESET}{BG_CHARCOAL}  {FG_GRAY}{custom_hint}{RESET}"
            lines.append(bg_ljust(line, width, BG_CHARCOAL))
        else:
            lines.append(f"  {FG_CUSTOM_ENV}{BOLD}{expand_marker} Custom Env{RESET}{FG_GRAY}{custom_count}{RESET}")

        # Preview modified fields when collapsed, or show description if none
        if not self.state.launch.custom_env_expanded:
            if custom_set > 0:
                previews = []
                for field in custom_fields:
                    if field.value:
                        val = str(field.value) if field.value else ""
                        val_str = val[:15] + "..." if len(val) > 15 else val
                        previews.append(f"{field.key}: {val_str}")
                if previews:
                    preview_text = ", ".join(previews)
                    lines.append(f"    {DIM}{FG_GRAY}{truncate_ansi(preview_text, width - 4)}{RESET}")
            else:
                lines.append(f"    {DIM}{FG_GRAY}custom environment variables{RESET}")

        # Custom Env fields
        result = self.render_section_fields(
            lines,
            custom_fields,
            self.state.launch.custom_env_expanded,
            LaunchField.CUSTOM_ENV_SECTION,
            self.state.launch.custom_env_cursor,
            width,
            FG_CUSTOM_ENV,
        )
        if result is not None:
            selected_field_start_line = result

        # Add spacing after expanded section
        if self.state.launch.custom_env_expanded:
            lines.append("")

        # Open config in editor entry (at bottom, less prominent)
        lines.append("")  # Spacer
        editor_cmd, editor_label = self.tui.resolve_editor_command()
        editor_label_display = editor_label or "VS Code"
        editor_available = editor_cmd is not None
        editor_selected = self.state.launch.current_field == LaunchField.OPEN_EDITOR

        if editor_selected:
            selected_field_start_line = len(lines)
            lines.append(
                bg_ljust(
                    f"  {BG_CHARCOAL}{FG_WHITE}\u2197 Open config in {editor_label_display}{RESET}"
                    f"{BG_CHARCOAL}  "
                    f"{(FG_GRAY if editor_available else FG_RED)}\u2022 "
                    f"{'enter: open' if editor_available else 'code CLI not found - set $EDITOR'}{RESET}",
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
            if selected_field_start_line < self.state.launch.scroll_pos:
                self.state.launch.scroll_pos = selected_field_start_line
            # Scroll down if selected field is below visible window
            elif selected_field_start_line >= self.state.launch.scroll_pos + form_height:
                self.state.launch.scroll_pos = selected_field_start_line - form_height + 1

            # Clamp scroll position
            self.state.launch.scroll_pos = max(0, min(self.state.launch.scroll_pos, max_scroll))

        # Render visible window instead of truncating
        if len(lines) > form_height:
            # Extract visible slice based on scroll position
            visible_lines = lines[self.state.launch.scroll_pos : self.state.launch.scroll_pos + form_height]
            # Pad if needed (shouldn't happen, but for safety)
            while len(visible_lines) < form_height:
                visible_lines.append("")
            lines = visible_lines
        else:
            # Form fits entirely, no scrolling needed
            while len(lines) < form_height:
                lines.append("")

        # Editor (if active) - always fits because we reserved space
        if field_info:
            field_key, field_value, cursor_pos = field_info

            # Build descriptive header for each field with background
            if field_key == "prompt":
                editor_color = FG_CLAUDE_ORANGE
                field_name = "Prompt"
                help_text = "initial prompt sent on launch"
            elif field_key == "system_prompt":
                editor_color = FG_CLAUDE_ORANGE
                field_name = "System Prompt"
                help_text = "instructions that guide behavior"
            elif field_key == "append_system_prompt":
                editor_color = FG_CLAUDE_ORANGE
                field_name = "Append System Prompt"
                help_text = "appends to Claude's default system prompt"
            elif field_key == "codex_system_prompt":
                editor_color = FG_CLAUDE_ORANGE
                field_name = "System Prompt"
                help_text = "instructions via developer_instructions flag"
            elif field_key == "gemini_system_prompt":
                editor_color = FG_CLAUDE_ORANGE
                field_name = "System Prompt"
                help_text = "instructions via GEMINI_SYSTEM_MD"
            elif field_key == "codex_args":
                editor_color = FG_CLAUDE_ORANGE
                field_name = "Codex Args"
                help_text = "raw flags passed to Codex CLI"
            elif field_key == "HCOM_CLAUDE_ARGS":
                editor_color = FG_CLAUDE_ORANGE
                field_name = "Claude Args"
                help_text = "raw flags passed to Claude CLI"
            # HCOM_TIMEOUT: hidden from TUI (internal, headless/vanilla only)
            elif field_key == "HCOM_SUBAGENT_TIMEOUT":
                editor_color = FG_CYAN
                field_name = "Subagent Timeout"
                help_text = "seconds before disconnecting idle subagent"
            elif field_key == "HCOM_TERMINAL":
                editor_color = FG_CYAN
                field_name = "Terminal"
                help_text = "default, preset, or custom (use {script} placeholder)"
            elif field_key == "HCOM_HINTS":
                editor_color = FG_CYAN
                field_name = "Hints"
                help_text = "text appended to all messages this instance receives"
            elif field_key == "HCOM_TAG":
                editor_color = FG_CYAN
                field_name = "Tag"
                help_text = "identifier to create groups with @-mention"
            elif field_key == "HCOM_AUTO_SUBSCRIBE":
                editor_color = FG_CYAN
                field_name = "Auto-Subscribe"
                help_text = "event notifications agents receive"
            elif field_key.startswith("HCOM_"):
                # Other HCOM fields
                editor_color = FG_CYAN
                field_name = field_key.replace("HCOM_", "").replace("_", " ").title()
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
            lines.append("")  # Blank line between header and input
            # Render editor with wrapping support
            editor_lines = render_text_input(field_value, cursor_pos, width, editor_content_rows, prefix="")
            lines.extend(editor_lines)
            # Separator after editor input
            lines.append(separator_line(width))
        else:
            # Separator at bottom when no editor
            lines.append(separator_line(width))

        return lines[:height]

    # Key handler methods for dispatch pattern
    def _handle_up(self):
        """Handle UP key navigation"""
        if self.state.launch.current_field == LaunchField.CLAUDE_SECTION:
            if self.state.launch.claude_cursor > -1:
                self.state.launch.claude_cursor -= 1
            else:
                self.state.launch.current_field = LaunchField.LAUNCH_BTN
        elif self.state.launch.current_field == LaunchField.HCOM_SECTION:
            if self.state.launch.hcom_cursor > -1:
                self.state.launch.hcom_cursor -= 1
            else:
                self.state.launch.current_field = LaunchField.CLAUDE_SECTION
                self.state.launch.claude_cursor = -1
        elif self.state.launch.current_field == LaunchField.CUSTOM_ENV_SECTION:
            if self.state.launch.custom_env_cursor > -1:
                self.state.launch.custom_env_cursor -= 1
            else:
                self.state.launch.current_field = LaunchField.HCOM_SECTION
                self.state.launch.hcom_cursor = -1
        elif self.state.launch.current_field == LaunchField.OPEN_EDITOR:
            self.state.launch.current_field = LaunchField.CUSTOM_ENV_SECTION
            self.state.launch.custom_env_cursor = -1
        else:
            fields = list(LaunchField)
            idx = fields.index(self.state.launch.current_field)
            self.state.launch.current_field = fields[(idx - 1) % len(fields)]

    def _handle_down(self):
        """Handle DOWN key navigation"""
        if self.state.launch.current_field == LaunchField.CLAUDE_SECTION:
            if self.state.launch.claude_cursor == -1 and not self.state.launch.claude_expanded:
                self.state.launch.current_field = LaunchField.HCOM_SECTION
                self.state.launch.hcom_cursor = -1
            elif self.state.launch.claude_expanded:
                max_idx = len(self.build_claude_fields()) - 1
                if self.state.launch.claude_cursor < max_idx:
                    self.state.launch.claude_cursor += 1
                else:
                    self.state.launch.current_field = LaunchField.HCOM_SECTION
                    self.state.launch.hcom_cursor = -1
        elif self.state.launch.current_field == LaunchField.HCOM_SECTION:
            if self.state.launch.hcom_cursor == -1 and not self.state.launch.hcom_expanded:
                self.state.launch.current_field = LaunchField.CUSTOM_ENV_SECTION
                self.state.launch.custom_env_cursor = -1
            elif self.state.launch.hcom_expanded:
                max_idx = len(self.build_hcom_fields()) - 1
                if self.state.launch.hcom_cursor < max_idx:
                    self.state.launch.hcom_cursor += 1
                else:
                    self.state.launch.current_field = LaunchField.CUSTOM_ENV_SECTION
                    self.state.launch.custom_env_cursor = -1
        elif self.state.launch.current_field == LaunchField.CUSTOM_ENV_SECTION:
            if self.state.launch.custom_env_cursor == -1 and not self.state.launch.custom_env_expanded:
                self.state.launch.current_field = LaunchField.OPEN_EDITOR
            elif self.state.launch.custom_env_expanded:
                max_idx = len(self.build_custom_env_fields()) - 1
                if self.state.launch.custom_env_cursor < max_idx:
                    self.state.launch.custom_env_cursor += 1
                else:
                    self.state.launch.current_field = LaunchField.OPEN_EDITOR
        else:
            fields = list(LaunchField)
            idx = fields.index(self.state.launch.current_field)
            self.state.launch.current_field = fields[(idx + 1) % len(fields)]
            if self.state.launch.current_field == LaunchField.CLAUDE_SECTION:
                self.state.launch.claude_cursor = -1
            elif self.state.launch.current_field == LaunchField.HCOM_SECTION:
                self.state.launch.hcom_cursor = -1
            elif self.state.launch.current_field == LaunchField.CUSTOM_ENV_SECTION:
                self.state.launch.custom_env_cursor = -1

    def _update_cursor(self, field_key: str, new_cursor: int):
        """Update cursor position for a field using registry"""
        if field_key == "prompt":
            _, cursor_attr = _PROMPT_STORAGE[self.state.launch.tool]
            setattr(self.state.launch, cursor_attr, new_cursor)
        elif field_key in ("system_prompt", "append_system_prompt", "codex_system_prompt", "gemini_system_prompt"):
            storage = _FIELD_STORAGE.get(field_key)
            if storage and storage != "tool_specific":
                _, maybe_cursor_attr, _ = storage
                if maybe_cursor_attr:
                    setattr(self.state.launch, maybe_cursor_attr, new_cursor)
        else:
            self.state.launch.config_field_cursors[field_key] = new_cursor

    def _get_current_field_obj(self):
        """Get field object at current cursor position"""
        if self.state.launch.current_field == LaunchField.CLAUDE_SECTION and self.state.launch.claude_cursor >= 0:
            fields = self.build_claude_fields()
            if self.state.launch.claude_cursor < len(fields):
                return fields[self.state.launch.claude_cursor]
        elif self.state.launch.current_field == LaunchField.HCOM_SECTION and self.state.launch.hcom_cursor >= 0:
            fields = self.build_hcom_fields()
            if self.state.launch.hcom_cursor < len(fields):
                return fields[self.state.launch.hcom_cursor]
        return None

    def _handle_left_right(self, key: str):
        """Handle LEFT/RIGHT key - cycle options, adjust count, or move cursor"""
        # TOOL field: cycle tool
        if self.state.launch.current_field == LaunchField.TOOL:
            options = list(self._tool_options)
            current = self.state.launch.tool if self.state.launch.tool in options else "claude"
            idx = options.index(current)
            new_idx = (idx + 1) if key == "RIGHT" else (idx - 1)
            self.state.launch.tool = options[new_idx % len(options)]
            if self.state.launch.tool not in RELEASED_BACKGROUND:
                self.state.launch.background = False
            return

        # COUNT field: adjust by ±1
        if self.state.launch.current_field == LaunchField.COUNT:
            try:
                current_count = int(self.state.launch.count) if self.state.launch.count else 1
                current_count = min(999, current_count + 1) if key == "RIGHT" else max(1, current_count - 1)
                self.state.launch.count = str(current_count)
            except ValueError:
                self.state.launch.count = "1"
            return

        # Field-based handling
        field_info = self.get_current_field_info()
        if not field_info:
            return

        field_key, field_value, cursor_pos = field_info
        field_obj = self._get_current_field_obj()

        # Cycle field handling
        if field_obj and field_obj.field_type == "cycle":
            self._handle_cycle_field(key, field_key, field_value, field_obj)
        elif field_obj and field_obj.field_type == "multi_cycle":
            self._handle_multi_cycle_field(key, field_obj)
        else:
            # Text field: move cursor
            new_cursor = (
                text_input_move_left(cursor_pos) if key == "LEFT" else text_input_move_right(field_value, cursor_pos)
            )
            self._update_cursor(field_key, new_cursor)

    def _handle_cycle_field(self, key: str, field_key: str, field_value: str, field_obj):
        """Handle LEFT/RIGHT on cycle field"""
        options = field_obj.options or []
        if not options:
            return

        # Special handling for HCOM_TERMINAL custom mode
        if field_key == "HCOM_TERMINAL" and field_value not in options:
            if not field_value:
                # Empty custom, cycle back to presets
                new_idx = len(options) - 2 if key == "LEFT" else 0
                self.state.config_edit[field_key] = options[new_idx] if options else "default"
                self.state.launch.config_field_cursors[field_key] = len(options[new_idx]) if options else 7
                self.tui.save_config_to_file()
            else:
                # Move cursor in custom command text
                new_cursor = (
                    text_input_move_left(self.state.launch.config_field_cursors.get(field_key, 0))
                    if key == "LEFT"
                    else text_input_move_right(field_value, self.state.launch.config_field_cursors.get(field_key, 0))
                )
                self.state.launch.config_field_cursors[field_key] = new_cursor
            return

        # Normal cycle through options
        if field_value in options:
            idx = options.index(field_value)
            new_idx = ((idx + 1) if key == "RIGHT" else (idx - 1)) % len(options)
        else:
            new_idx = 0
        new_value = options[new_idx]

        # For HCOM_TERMINAL 'custom', clear value to enter edit mode
        if field_key == "HCOM_TERMINAL" and new_value == "custom":
            self.state.config_edit[field_key] = ""
            self.state.launch.config_field_cursors[field_key] = 0
        else:
            self.state.config_edit[field_key] = new_value
            self.state.launch.config_field_cursors[field_key] = len(new_value)
        self.tui.save_config_to_file()

    def _handle_multi_cycle_field(self, key: str, field_obj):
        """Handle LEFT/RIGHT on multi_cycle field"""
        options = field_obj.options or []
        if not options:
            return
        field_key = field_obj.key
        current_idx = self.state.launch.multi_cycle_indices.get(field_key, 0)
        new_idx = ((current_idx + 1) if key == "RIGHT" else (current_idx - 1)) % len(options)
        self.state.launch.multi_cycle_indices[field_key] = new_idx

    def _handle_enter(self):
        """Handle ENTER key - expand/collapse, toggle, cycle, launch"""
        lf = self.state.launch.current_field

        # Section header toggles
        if lf == LaunchField.CLAUDE_SECTION and self.state.launch.claude_cursor == -1:
            self.state.launch.claude_expanded = not self.state.launch.claude_expanded
        elif lf == LaunchField.HCOM_SECTION and self.state.launch.hcom_cursor == -1:
            self.state.launch.hcom_expanded = not self.state.launch.hcom_expanded
        elif lf == LaunchField.CUSTOM_ENV_SECTION and self.state.launch.custom_env_cursor == -1:
            self.state.launch.custom_env_expanded = not self.state.launch.custom_env_expanded
        # Section field handling
        elif lf == LaunchField.CLAUDE_SECTION and self.state.launch.claude_cursor >= 0:
            self._handle_enter_claude_field()
        elif lf == LaunchField.HCOM_SECTION and self.state.launch.hcom_cursor >= 0:
            self._handle_enter_hcom_field()
        elif lf == LaunchField.LAUNCH_BTN:
            self.do_launch()
        elif lf == LaunchField.OPEN_EDITOR:
            self.tui.open_config_in_editor()

    def _handle_enter_claude_field(self):
        """Handle ENTER on a claude section field"""
        fields = self.build_claude_fields()
        if self.state.launch.claude_cursor >= len(fields):
            return
        field = fields[self.state.launch.claude_cursor]
        if field.field_type == "checkbox" and field.key == "background":
            self.state.launch.background = not self.state.launch.background
            self.tui.save_launch_state()
        elif field.field_type == "cycle":
            options = field.options or []
            if options:
                current = self.state.config_edit.get(field.key, options[0])
                idx = options.index(current) if current in options else 0
                new_value = options[(idx + 1) % len(options)]
                self.state.config_edit[field.key] = new_value
                self.state.launch.config_field_cursors[field.key] = len(new_value)
                self.tui.save_config_to_file()

    def _handle_enter_hcom_field(self):
        """Handle ENTER on an hcom section field"""
        fields = self.build_hcom_fields()
        if self.state.launch.hcom_cursor >= len(fields):
            return
        field = fields[self.state.launch.hcom_cursor]
        if field.field_type == "checkbox":
            current = self.state.config_edit.get(field.key, "")
            new_value = "0" if current == "1" else "1"
            self.state.config_edit[field.key] = new_value
            self.tui.save_config_to_file()

    def _handle_backspace(self):
        """Handle BACKSPACE - delete char before cursor"""
        field_info = self.get_current_field_info()
        if not field_info:
            return
        field_key, field_value, cursor_pos = field_info
        if field_key == "HCOM_CODEX_SANDBOX_MODE":
            return
        new_value, new_cursor = text_input_backspace(field_value, cursor_pos)
        self.update_field(field_key, new_value, new_cursor)

    def _handle_esc(self):
        """Handle ESC - clear field or collapse section"""
        lf = self.state.launch.current_field

        if lf == LaunchField.TOOL:
            self.state.launch.tool = "claude"
            return

        if lf == LaunchField.COUNT:
            self.state.launch.count = "1"
            return

        if lf == LaunchField.CLAUDE_SECTION:
            if self.state.launch.claude_cursor >= 0:
                self._clear_claude_field()
            else:
                self.state.launch.claude_expanded = False
                self.state.launch.claude_cursor = -1
        elif lf == LaunchField.HCOM_SECTION:
            if self.state.launch.hcom_cursor >= 0:
                self._clear_section_field(self.build_hcom_fields(), self.state.launch.hcom_cursor)
            else:
                self.state.launch.hcom_expanded = False
                self.state.launch.hcom_cursor = -1
        elif lf == LaunchField.CUSTOM_ENV_SECTION:
            if self.state.launch.custom_env_cursor >= 0:
                self._clear_section_field(self.build_custom_env_fields(), self.state.launch.custom_env_cursor)
            else:
                self.state.launch.custom_env_expanded = False
                self.state.launch.custom_env_cursor = -1

    def _clear_claude_field(self):
        """Clear field in claude section"""
        fields = self.build_claude_fields()
        if self.state.launch.claude_cursor >= len(fields):
            return
        field = fields[self.state.launch.claude_cursor]

        # Use update_field for known fields, direct clear for args
        if field.key in (
            "prompt",
            "system_prompt",
            "append_system_prompt",
            "allowed_tools",
            "codex_system_prompt",
            "gemini_system_prompt",
        ):
            self.update_field(field.key, "", 0)
        elif field.key == "claude_args":
            self.state.config_edit["HCOM_CLAUDE_ARGS"] = ""
            self.state.launch.config_field_cursors["HCOM_CLAUDE_ARGS"] = 0
            self.tui.save_config_to_file()
            self.tui.load_launch_state()
        elif field.key == "codex_args":
            self.state.config_edit["HCOM_CODEX_ARGS"] = ""
            self.state.launch.config_field_cursors["HCOM_CODEX_ARGS"] = 0
            self.tui.save_config_to_file()
        elif field.key == "gemini_args":
            self.state.config_edit["HCOM_GEMINI_ARGS"] = ""
            self.state.launch.config_field_cursors["HCOM_GEMINI_ARGS"] = 0
            self.tui.save_config_to_file()

    def _clear_section_field(self, fields: list, cursor: int):
        """Clear a config field in hcom/custom_env section"""
        if cursor >= len(fields):
            return
        field = fields[cursor]
        self.state.config_edit[field.key] = ""
        self.state.launch.config_field_cursors[field.key] = 0
        self.tui.save_config_to_file()

    def _handle_ctrl_r(self):
        """Handle CTRL_R - reset config with confirmation"""
        is_confirming = (
            self.state.confirm.pending_reset
            and (time.time() - self.state.confirm.pending_reset_time) <= self.tui.CONFIRMATION_TIMEOUT
        )

        if is_confirming:
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
                self.state.confirm.pending_reset = False
        else:
            self.state.confirm.pending_reset = True
            self.state.confirm.pending_reset_time = time.time()
            self.tui.flash(
                f"{FG_WHITE}Confirm backup + reset config to defaults? (Ctrl+R again){RESET}",
                duration=self.tui.CONFIRMATION_FLASH_DURATION,
                color="white",
            )

    def _handle_text_input(self, key: str):
        """Handle SPACE and printable character input"""
        char = " " if key == "SPACE" else key
        field_info = self.get_current_field_info()
        if not field_info:
            return

        field_key, field_value, cursor_pos = field_info

        # Handle multi_cycle fields: SPACE toggles current option
        if key == "SPACE":
            field_obj = self._get_current_field_obj()
            if field_obj and field_obj.field_type == "multi_cycle":
                options = field_obj.options or []
                current_idx = self.state.launch.multi_cycle_indices.get(field_key, 0)
                current_option = options[current_idx] if options else ""

                # Parse existing value
                current_value = str(field_value) if field_value else ""
                selected = [x.strip() for x in current_value.split(",") if x.strip()]

                # Toggle: add if not present, remove if present
                if current_option in selected:
                    selected.remove(current_option)
                else:
                    selected.append(current_option)

                new_value = ",".join(selected)
                self.update_field(field_key, new_value, len(new_value))

                # Auto-advance to next option
                if options:
                    self.state.launch.multi_cycle_indices[field_key] = (current_idx + 1) % len(options)
                return

        # Skip cycle fields (they use LEFT/RIGHT to cycle, not text input)
        if field_key == "HCOM_CODEX_SANDBOX_MODE":
            return

        # Validate for special fields
        if field_key == "HCOM_TAG":
            override = cast(Dict[str, Any], CONFIG_FIELD_OVERRIDES.get(field_key, {}))
            allowed_pattern = override.get("allowed_chars")
            if allowed_pattern:
                test_value = field_value[:cursor_pos] + char + field_value[cursor_pos:]
                if not re.match(allowed_pattern, test_value):
                    return

        new_value, new_cursor = text_input_insert(field_value, cursor_pos, char)
        self.update_field(field_key, new_value, new_cursor)

    def handle_key(self, key: str):
        """Handle keys in Launch mode using dispatch pattern"""
        if key == "UP":
            return self._handle_up()
        elif key == "DOWN":
            return self._handle_down()
        elif key in ("LEFT", "RIGHT"):
            return self._handle_left_right(key)
        elif key == "ENTER":
            return self._handle_enter()
        elif key == "BACKSPACE":
            return self._handle_backspace()
        elif key == "ESC":
            return self._handle_esc()
        elif key == "CTRL_R":
            return self._handle_ctrl_r()
        elif key == "SPACE" or (key and len(key) == 1 and key.isprintable()):
            return self._handle_text_input(key)

    def get_command_preview(self) -> str:
        """Build preview using spec (matches exactly what will be launched)"""
        try:
            tool = self.state.launch.tool if self.state.launch.tool in self._tool_options else "claude"
            count = self.state.launch.count if self.state.launch.count else "1"

            # Get tool-specific config args
            args_key = f"HCOM_{tool.upper()}_ARGS"
            args_str = self.state.config_edit.get(args_key, "")
            args = shlex.split(args_str) if args_str else []
            parts = [f"hcom {count}", tool]
            if args:
                parts.append(shlex.join(args))

            # Get tool-specific prompt
            if tool == "claude":
                tool_prompt = self.state.launch.prompt
            elif tool == "gemini":
                tool_prompt = self.state.launch.gemini_prompt
            else:
                tool_prompt = self.state.launch.codex_prompt

            if tool_prompt:
                parts.append(f"(prompt: {tool_prompt[:30]}{'...' if len(tool_prompt) > 30 else ''})")
            else:
                # Check if args provide initial prompt
                has_args_prompt = False
                if tool == "claude":
                    claude_spec = resolve_claude_args(None, args_str if args_str else None)
                    has_args_prompt = bool(claude_spec.positional_tokens)
                elif tool == "gemini":
                    gemini_spec = resolve_gemini_args(None, args_str if args_str else None)
                    has_args_prompt = bool(gemini_spec.positional_tokens) or gemini_spec.has_flag(
                        ["-i", "--prompt-interactive"]
                    )
                elif tool == "codex":
                    codex_spec = resolve_codex_args(None, args_str if args_str else None)
                    has_args_prompt = bool(codex_spec.positional_tokens)
                if not has_args_prompt:
                    parts.append(f"{FG_GRAY}(no prompt){RESET}")

            if tool != "claude":
                return " ".join(parts)

            # Load spec and update with form values (same logic as do_launch)
            claude_args_str = self.state.config_edit.get("HCOM_CLAUDE_ARGS", "")
            spec = resolve_claude_args(None, claude_args_str if claude_args_str else None)

            # Update spec with background and prompt
            spec = spec.update(
                background=self.state.launch.background,
                prompt=self.state.launch.prompt,
            )

            # Build tokens, filtering out existing system prompts from clean_tokens
            tokens = []
            skip_next = False
            for i, token in enumerate(spec.clean_tokens):
                if skip_next:
                    skip_next = False
                    continue
                token_lower = token.lower()
                # Skip system prompt flags and their values
                if token_lower in ("--system-prompt", "--append-system-prompt"):
                    # Only skip next token if it's not another flag (it's the value)
                    if i + 1 < len(spec.clean_tokens):
                        next_token = spec.clean_tokens[i + 1]
                        if not next_token.startswith("-"):
                            skip_next = True
                    continue
                if token_lower.startswith(("--system-prompt=", "--append-system-prompt=")):
                    continue
                tokens.append(token)

            # Add UI state system prompts
            if self.state.launch.system_prompt:
                tokens.extend(["--system-prompt", self.state.launch.system_prompt])
            if self.state.launch.append_system_prompt:
                tokens.extend(["--append-system-prompt", self.state.launch.append_system_prompt])

            # Re-parse to get proper spec
            spec = resolve_claude_args(tokens, None)

            # Build preview
            parts = []

            # Environment variables (read from config_fields - source of truth)
            env_parts = []
            tag = self.state.config_edit.get("HCOM_TAG", "")
            if tag:
                tag_display = tag if len(tag) <= 15 else tag[:12] + "..."
                env_parts.append(f"HCOM_TAG={tag_display}")
            if env_parts:
                parts.append(" ".join(env_parts))

            # Base command
            parts.append(f"hcom {count}")

            # Claude args from spec (truncate long values for preview)
            tokens = spec.rebuild_tokens()
            if tokens:
                preview_tokens = []
                for token in tokens:
                    if len(token) > 30:
                        preview_tokens.append(f'"{token[:27]}..."')
                    elif " " in token:
                        preview_tokens.append(f'"{token}"')
                    else:
                        preview_tokens.append(token)
                parts.append("claude " + " ".join(preview_tokens))

            return " ".join(parts)
        except Exception:
            return "(preview unavailable - check HCOM_CLAUDE_ARGS)"

    def get_current_field_info(self) -> tuple[str, str, int] | None:
        """Get (field_key, field_value, cursor_pos) for currently selected field, or None"""
        if self.state.launch.current_field == LaunchField.CLAUDE_SECTION and self.state.launch.claude_cursor >= 0:
            fields = self.build_claude_fields()
            if self.state.launch.claude_cursor < len(fields):
                field = fields[self.state.launch.claude_cursor]
                if field.key == "prompt":
                    tool = self.state.launch.tool
                    if tool == "claude":
                        if self.state.launch.prompt_cursor > len(self.state.launch.prompt):
                            self.state.launch.prompt_cursor = len(self.state.launch.prompt)
                        return (
                            "prompt",
                            self.state.launch.prompt,
                            self.state.launch.prompt_cursor,
                        )
                    elif tool == "gemini":
                        if self.state.launch.gemini_prompt_cursor > len(self.state.launch.gemini_prompt):
                            self.state.launch.gemini_prompt_cursor = len(self.state.launch.gemini_prompt)
                        return (
                            "prompt",
                            self.state.launch.gemini_prompt,
                            self.state.launch.gemini_prompt_cursor,
                        )
                    else:  # codex
                        if self.state.launch.codex_prompt_cursor > len(self.state.launch.codex_prompt):
                            self.state.launch.codex_prompt_cursor = len(self.state.launch.codex_prompt)
                        return (
                            "prompt",
                            self.state.launch.codex_prompt,
                            self.state.launch.codex_prompt_cursor,
                        )
                elif field.key == "system_prompt":
                    if self.state.launch.system_prompt_cursor > len(self.state.launch.system_prompt):
                        self.state.launch.system_prompt_cursor = len(self.state.launch.system_prompt)
                    return (
                        "system_prompt",
                        self.state.launch.system_prompt,
                        self.state.launch.system_prompt_cursor,
                    )
                elif field.key == "append_system_prompt":
                    if self.state.launch.append_system_prompt_cursor > len(self.state.launch.append_system_prompt):
                        self.state.launch.append_system_prompt_cursor = len(self.state.launch.append_system_prompt)
                    return (
                        "append_system_prompt",
                        self.state.launch.append_system_prompt,
                        self.state.launch.append_system_prompt_cursor,
                    )
                elif field.key == "allowed_tools":
                    # multi_cycle field - return current value
                    value = self.state.launch.allowed_tools
                    return ("allowed_tools", value, len(value))
                elif field.key == "codex_system_prompt":
                    if self.state.launch.codex_system_prompt_cursor > len(self.state.launch.codex_system_prompt):
                        self.state.launch.codex_system_prompt_cursor = len(self.state.launch.codex_system_prompt)
                    return (
                        "codex_system_prompt",
                        self.state.launch.codex_system_prompt,
                        self.state.launch.codex_system_prompt_cursor,
                    )
                elif field.key == "HCOM_CODEX_SANDBOX_MODE":
                    # Cycle field - return current value with cursor at end
                    value = self.state.config_edit.get("HCOM_CODEX_SANDBOX_MODE", "workspace")
                    return (
                        "HCOM_CODEX_SANDBOX_MODE",
                        value,
                        len(value),
                    )
                elif field.key == "gemini_system_prompt":
                    if self.state.launch.gemini_system_prompt_cursor > len(self.state.launch.gemini_system_prompt):
                        self.state.launch.gemini_system_prompt_cursor = len(self.state.launch.gemini_system_prompt)
                    return (
                        "gemini_system_prompt",
                        self.state.launch.gemini_system_prompt,
                        self.state.launch.gemini_system_prompt_cursor,
                    )
                elif field.key == "claude_args":
                    value = self.state.config_edit.get("HCOM_CLAUDE_ARGS", "")
                    cursor = self.state.launch.config_field_cursors.get("HCOM_CLAUDE_ARGS", len(value))
                    cursor = min(cursor, len(value))
                    self.state.launch.config_field_cursors["HCOM_CLAUDE_ARGS"] = cursor
                    return ("HCOM_CLAUDE_ARGS", value, cursor)
                elif field.key == "codex_args":
                    value = self.state.config_edit.get("HCOM_CODEX_ARGS", "")
                    cursor = self.state.launch.config_field_cursors.get("HCOM_CODEX_ARGS", len(value))
                    cursor = min(cursor, len(value))
                    self.state.launch.config_field_cursors["HCOM_CODEX_ARGS"] = cursor
                    return ("HCOM_CODEX_ARGS", value, cursor)
                elif field.key == "gemini_args":
                    value = self.state.config_edit.get("HCOM_GEMINI_ARGS", "")
                    cursor = self.state.launch.config_field_cursors.get("HCOM_GEMINI_ARGS", len(value))
                    cursor = min(cursor, len(value))
                    self.state.launch.config_field_cursors["HCOM_GEMINI_ARGS"] = cursor
                    return ("HCOM_GEMINI_ARGS", value, cursor)
        elif self.state.launch.current_field == LaunchField.HCOM_SECTION and self.state.launch.hcom_cursor >= 0:
            fields = self.build_hcom_fields()
            if self.state.launch.hcom_cursor < len(fields):
                field = fields[self.state.launch.hcom_cursor]
                # Don't show editor for checkbox fields - they toggle on Enter
                if field.field_type == "checkbox":
                    return None
                value = self.state.config_edit.get(field.key, "")
                cursor = self.state.launch.config_field_cursors.get(field.key, len(value))
                cursor = min(cursor, len(value))
                self.state.launch.config_field_cursors[field.key] = cursor
                return (field.key, value, cursor)
        elif (
            self.state.launch.current_field == LaunchField.CUSTOM_ENV_SECTION
            and self.state.launch.custom_env_cursor >= 0
        ):
            fields = self.build_custom_env_fields()
            if self.state.launch.custom_env_cursor < len(fields):
                field = fields[self.state.launch.custom_env_cursor]
                value = self.state.config_edit.get(field.key, "")
                cursor = self.state.launch.config_field_cursors.get(field.key, len(value))
                cursor = min(cursor, len(value))
                self.state.launch.config_field_cursors[field.key] = cursor
                return (field.key, value, cursor)
        return None

    def update_field(self, field_key: str, new_value: str, new_cursor: int):
        """Update a launch field with new value and cursor position.

        Uses _FIELD_STORAGE registry to determine storage location and save action.
        """
        storage = _FIELD_STORAGE.get(field_key)

        if storage == "tool_specific":
            # Prompt field varies by current tool
            value_attr, cursor_attr = _PROMPT_STORAGE[self.state.launch.tool]
            setattr(self.state.launch, value_attr, new_value)
            setattr(self.state.launch, cursor_attr, new_cursor)
            self.tui.save_launch_state()
        elif storage is not None:
            # Known field with explicit storage config
            storage_value_attr, storage_cursor_attr, save_action = storage
            if storage_value_attr:
                setattr(self.state.launch, storage_value_attr, new_value)
                if storage_cursor_attr:  # multi_cycle fields have cursor_attr=None
                    setattr(self.state.launch, storage_cursor_attr, new_cursor)
            else:
                self.state.config_edit[field_key] = new_value
                self.state.launch.config_field_cursors[field_key] = new_cursor

            if save_action == "launch":
                self.tui.save_launch_state()
            elif save_action == "config_reload":
                self.tui.save_config_to_file()
                self.tui.load_launch_state()
            else:  # "config"
                self.tui.save_config_to_file()
        else:
            # Default: generic config field
            self.state.config_edit[field_key] = new_value
            self.state.launch.config_field_cursors[field_key] = new_cursor
            self.tui.save_config_to_file()

    def build_claude_fields(self) -> List[Field]:
        """Build Claude section fields from memory vars"""
        tool = self.state.launch.tool
        if tool == "claude":
            return [
                Field(
                    "prompt",
                    "Prompt",
                    "text",
                    self.state.launch.prompt,
                    hint="text string",
                ),
                Field(
                    "system_prompt",
                    "System Prompt",
                    "text",
                    self.state.launch.system_prompt,
                    hint="text string",
                ),
                Field(
                    "append_system_prompt",
                    "Append System Prompt",
                    "text",
                    self.state.launch.append_system_prompt,
                    hint="text string",
                ),
                Field(
                    "allowed_tools",
                    "Allowed Tools",
                    "multi_cycle",
                    self.state.launch.allowed_tools,
                    options=["Bash", "Edit", "Write", "WebFetch", "WebSearch", "NotebookEdit"],
                    hint="←→ cycle, SPACE to add",
                ),
                Field(
                    "background",
                    "Headless",
                    "checkbox",
                    self.state.launch.background,
                    hint="enter to toggle",
                ),
                Field(
                    "claude_args",
                    "Claude Args",
                    "text",
                    self.state.config_edit.get("HCOM_CLAUDE_ARGS", ""),
                    hint="flags string",
                ),
            ]

        if tool == "codex":
            # Codex: similar to Claude with system prompt and sandbox mode
            sandbox_hints = {
                "workspace": "←→ normal codex - edits auto-approved",
                "untrusted": "←→ read-only - edits need Y/n approval",
                "danger-full-access": "←→ full access (needed for Codex launches)",
                "none": "←→ raw codex (hcom may not work)",
            }
            current_sandbox_mode = self.state.config_edit.get("HCOM_CODEX_SANDBOX_MODE", "workspace")
            if current_sandbox_mode == "full-auto":
                current_sandbox_mode = "danger-full-access"
                self.state.config_edit["HCOM_CODEX_SANDBOX_MODE"] = current_sandbox_mode
            fields = [
                Field(
                    "prompt",
                    "Prompt",
                    "text",
                    self.state.launch.codex_prompt,
                    hint="text string",
                ),
                Field(
                    "codex_system_prompt",
                    "System Prompt",
                    "text",
                    self.state.launch.codex_system_prompt,
                    hint="text string",
                ),
                Field(
                    "HCOM_CODEX_SANDBOX_MODE",
                    "Sandbox",
                    "cycle",
                    current_sandbox_mode,
                    options=["workspace", "untrusted", "danger-full-access", "none"],
                    hint=sandbox_hints.get(current_sandbox_mode, ""),
                ),
            ]
            if "codex" in RELEASED_BACKGROUND:
                fields.append(
                    Field(
                        "background",
                        "Headless",
                        "checkbox",
                        self.state.launch.background,
                        hint="enter to toggle",
                    )
                )
            fields.append(
                Field(
                    "codex_args",
                    "Codex Args",
                    "text",
                    self.state.config_edit.get("HCOM_CODEX_ARGS", ""),
                    hint="flags string",
                )
            )
            return fields

        # Gemini: prompt injected via PTY (or headless), system prompt via GEMINI_SYSTEM_MD.
        fields = [
            Field(
                "prompt",
                "Prompt",
                "text",
                self.state.launch.gemini_prompt,
                hint="text string",
            ),
            Field(
                "gemini_system_prompt",
                "System Prompt",
                "text",
                self.state.launch.gemini_system_prompt,
                hint="text string",
            ),
        ]
        if "gemini" in RELEASED_BACKGROUND:
            fields.append(
                Field(
                    "background",
                    "Headless",
                    "checkbox",
                    self.state.launch.background,
                    hint="enter to toggle",
                )
            )
        fields.append(
            Field(
                "gemini_args",
                "Gemini Args",
                "text",
                self.state.config_edit.get("HCOM_GEMINI_ARGS", ""),
                hint="flags string",
            )
        )
        return fields

    def build_hcom_fields(self) -> List[Field]:
        """Build HCOM section fields - always show all expected HCOM vars"""
        # Extract expected keys from DEFAULT_CONFIG_DEFAULTS (excluding tool-specific vars)
        # Tool args and system prompts are shown in the tool-specific section, not here
        # Tool-specific vars shown in tool section, hidden vars not shown at all
        tool_specific_prefixes = (
            "HCOM_CLAUDE_ARGS=",
            "HCOM_GEMINI_ARGS=",
            "HCOM_CODEX_ARGS=",
            "HCOM_GEMINI_SYSTEM_PROMPT=",
            "HCOM_CODEX_SYSTEM_PROMPT=",
        )
        # HCOM_TIMEOUT: only for headless/vanilla claude, hidden from TUI
        hidden_keys = {"HCOM_TIMEOUT"}
        expected_keys = [
            line.split("=")[0]
            for line in DEFAULT_CONFIG_DEFAULTS
            if line.startswith("HCOM_")
            and not any(line.startswith(p) for p in tool_specific_prefixes)
            and line.split("=")[0] not in hidden_keys
        ]

        fields = []
        for key in expected_keys:
            display_name = key.replace("HCOM_", "").replace("_", " ").title()
            override = cast(Dict[str, Any], CONFIG_FIELD_OVERRIDES.get(key, {}))
            field_type = str(override.get("type", "text"))
            options = override.get("options")
            if callable(options):
                options = options()
            hint_val = str(override.get("hint", ""))
            value = self.state.config_edit.get(key, "")
            if key == "HCOM_CODEX_SANDBOX_MODE":
                sandbox_hints = {
                    "workspace": "←→ normal codex - edits auto-approved",
                    "untrusted": "←→ read-only - edits need Y/n approval",
                    "danger-full-access": "←→ full access (needed for Codex launches)",
                    "none": "←→ raw codex (hcom may not work)",
                }
                hint_val = sandbox_hints.get(value or "workspace", hint_val)
            elif key == "HCOM_AUTO_SUBSCRIBE":
                # Dynamic hint: explain the currently previewed option
                sub_hints = {
                    "collision": "← when agents edit same file within 20s →",
                    "created": "← when new agents join →",
                    "stopped": "← when agents exit →",
                    "blocked": "← when agents need user approval →",
                }
                cycle_idx = self.state.launch.multi_cycle_indices.get(key, 0)
                sub_options = options or ["collision", "created", "stopped", "blocked"]
                current_opt = sub_options[cycle_idx] if cycle_idx < len(sub_options) else "collision"
                hint_val = sub_hints.get(current_opt, hint_val)
                display_name = "Auto-Subscribe"
            fields.append(
                Field(
                    key,
                    display_name,
                    cast(Any, field_type),
                    value,
                    options if isinstance(options, list) or options is None else None,
                    hint_val,
                )
            )

        # Also include any extra HCOM_* vars from config_fields (user-added)
        tool_specific_keys = {
            "HCOM_CLAUDE_ARGS",
            "HCOM_GEMINI_ARGS",
            "HCOM_CODEX_ARGS",
            "HCOM_GEMINI_SYSTEM_PROMPT",
            "HCOM_CODEX_SYSTEM_PROMPT",
        }
        for key in sorted(self.state.config_edit.keys()):
            if (
                key.startswith("HCOM_")
                and key not in tool_specific_keys
                and key not in expected_keys
                and key not in hidden_keys
            ):
                display_name = key.replace("HCOM_", "").replace("_", " ").title()
                override = cast(Dict[str, Any], CONFIG_FIELD_OVERRIDES.get(key, {}))
                field_type = str(override.get("type", "text"))
                options = override.get("options")
                if callable(options):
                    options = options()
                hint_val = str(override.get("hint", ""))
                fields.append(
                    Field(
                        key,
                        display_name,
                        cast(Any, field_type),
                        self.state.config_edit.get(key, ""),
                        options if isinstance(options, list) or options is None else None,
                        hint_val,
                    )
                )

        return fields

    def build_custom_env_fields(self) -> List[Field]:
        """Build Custom Env section fields from config_fields"""
        return [
            Field(key, key, "text", self.state.config_edit.get(key, ""))
            for key in sorted(self.state.config_edit.keys())
            if not key.startswith("HCOM_")
        ]

    def render_section_fields(
        self,
        lines: List[str],
        fields: List[Field],
        expanded: bool,
        section_field: LaunchField,
        section_cursor: int,
        width: int,
        color: str,
    ) -> int | None:
        """Render fields for an expandable section (extracted helper)

        Returns selected_field_start_line if a field is selected, None otherwise.
        """
        selected_field_start_line = None

        if expanded or (self.state.launch.current_field == section_field and section_cursor >= 0):
            visible_fields = fields if expanded else fields[:3]
            for i, field in enumerate(visible_fields):
                field_selected = self.state.launch.current_field == section_field and section_cursor == i
                if field_selected:
                    selected_field_start_line = len(lines)
                lines.append(self.render_field(field, field_selected, width, color))
            if not expanded and len(fields) > 3:
                lines.append(f"{FG_GRAY}    +{len(fields) - 3} more • enter to expand{RESET}")

        return selected_field_start_line

    def render_field(self, field: Field, selected: bool, width: int, value_color: str | None = None) -> str:
        """Render a single field line"""
        indent = "    "
        # Default to standard orange if not specified
        if value_color is None:
            value_color = FG_ORANGE

        # Format value based on type
        # For Claude fields, use cached defaults from HCOM_CLAUDE_ARGS (Claude tools only).
        if self.state.launch.tool == "claude" and field.key in (
            "prompt",
            "system_prompt",
            "append_system_prompt",
            "background",
        ):
            default_prompt, default_system, default_append, default_background = self._get_claude_defaults()
            default = {
                "prompt": default_prompt,
                "system_prompt": default_system,
                "append_system_prompt": default_append,
                "background": default_background,
            }[field.key]
        elif field.key in (
            "prompt",
            "system_prompt",
            "append_system_prompt",
            "background",
        ):
            default = ""
        else:
            default = CONFIG_DEFAULTS.get(field.key, "")

        # Check if field has validation error
        has_error = field.key in self.state.launch.validation_errors

        if field.field_type == "checkbox":
            # Handle both boolean (Claude section) and string '1'/'0' (HCOM section)
            is_checked = field.value is True or field.value == "1"
            check = "●" if is_checked else "○"
            # Color if differs from default
            default_checked = default is True or default == "1"
            is_modified = is_checked != default_checked
            value_str = f"{value_color if is_modified else FG_WHITE}{check}{RESET}"
        elif field.field_type == "text":
            if field.value:
                # Has value - color only if different from default (normalize quotes and whitespace)
                field_value_normalized = str(field.value).strip().strip("'\"").strip()
                default_normalized = str(default).strip().strip("'\"").strip()
                is_modified = field_value_normalized != default_normalized
                color = value_color if is_modified else FG_WHITE
                # Mask sensitive values (tokens)
                display_value = field.value
                if field.key == "HCOM_RELAY_TOKEN" and field.value:
                    token_val = str(field.value)
                    display_value = f"{token_val[:4]}***" if len(token_val) > 4 else "***"
                value_str = f"{color}{display_value}{RESET}"
            else:
                # Empty - check what runtime will actually use
                field_value_normalized = str(field.value).strip().strip("'\"").strip()
                default_normalized = str(default).strip().strip("'\"").strip()
                # Runtime uses empty if field doesn't auto-revert to default
                # For HCOM_CLAUDE_ARGS and Prompt, empty stays empty (doesn't use default)
                runtime_reverts_to_default = field.key not in (
                    "HCOM_CLAUDE_ARGS",
                    "HCOM_AUTO_SUBSCRIBE",
                    "prompt",
                )

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
        elif field.field_type == "cycle":
            # Special handling for HCOM_TERMINAL with availability info
            if field.key == "HCOM_TERMINAL":
                from ..terminal import get_available_presets

                presets = get_available_presets()
                preset_names = [name for name, _ in presets]
                preset_avail = {name: avail for name, avail in presets}

                if field.value and field.value not in preset_names:
                    # Custom command mode (value contains {script})
                    is_modified = True  # Custom is always modified
                    color = value_color
                    value_str = f"{color}{field.value}{RESET}"
                elif field.value:
                    # Preset value
                    is_available = preset_avail.get(str(field.value), True)
                    field_value_normalized = str(field.value).strip()
                    default_normalized = str(default).strip()
                    is_modified = field_value_normalized != default_normalized
                    color = value_color if is_modified else FG_WHITE
                    if not is_available:
                        value_str = f"{FG_GRAY}{field.value} (not installed){RESET}"
                    else:
                        value_str = f"{color}{field.value}{RESET}"
                else:
                    # Empty - in custom mode waiting for input
                    value_str = f"{FG_GRAY}(custom: use {{script}} placeholder){RESET}"
            else:
                # Generic cycle field
                if field.value:
                    field_value_normalized = str(field.value).strip().strip("'\"")
                    default_normalized = str(default).strip().strip("'\"")
                    is_modified = field_value_normalized != default_normalized
                    color = value_color if is_modified else FG_WHITE
                    value_str = f"{color}{field.value}{RESET}"
                else:
                    value_str = f"{FG_WHITE}(default: {default}){RESET}" if default else f"{FG_WHITE}(empty){RESET}"
        elif field.field_type == "multi_cycle":
            # Multi-cycle field: show selected values, preview only when focused
            chosen_items = [x.strip() for x in str(field.value).split(",") if x.strip()] if field.value else []

            if chosen_items:
                # Field-specific defaults
                if field.key == "HCOM_AUTO_SUBSCRIBE":
                    is_modified = set(chosen_items) != {"collision"}  # Default is collision
                else:
                    is_modified = True  # Any selection = modified (e.g., allowed_tools default is empty)
                color = value_color if is_modified else FG_WHITE
                value_str = f"{color}{','.join(chosen_items)}{RESET}"
            else:
                value_str = f"{FG_GRAY}(none){RESET}"

            # Only show preview picker when field is selected
            if selected:
                options = field.options or []
                field_key = field.key
                cycle_idx = self.state.launch.multi_cycle_indices.get(field_key, 0)
                current_preview = options[cycle_idx] if options else ""
                preview_in_chosen = current_preview in chosen_items
                # Use BG_CHARCOAL to maintain background, no RESET until end
                if preview_in_chosen:
                    preview_part = f"{BG_CHARCOAL} │ {FG_GREEN}✓ {current_preview}"
                else:
                    preview_part = f"{BG_CHARCOAL} │ {FG_CYAN}+ {current_preview}"
                value_str += preview_part
        else:  # numeric
            if field.value:
                # Has value - color only if different from default (normalize quotes)
                field_value_normalized = str(field.value).strip().strip("'\"")
                default_normalized = str(default).strip().strip("'\"")
                is_modified = field_value_normalized != default_normalized
                color = value_color if is_modified else FG_WHITE
                value_str = f"{color}{field.value}{RESET}"
            else:
                # Timeout fields: empty → runtime uses default → NOT modified
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
        if self.state.launch.current_field == LaunchField.TOOL:
            return f"{FG_GRAY}tab: switch  ←→: cycle  esc: reset to default  ctrl+r: reset config{RESET}"
        # Count field
        if self.state.launch.current_field == LaunchField.COUNT:
            return f"{FG_GRAY}tab: switch  ←→: adjust  esc: reset to 1  ctrl+r: reset config{RESET}"

        # Launch button
        elif self.state.launch.current_field == LaunchField.LAUNCH_BTN:
            return f"{FG_GRAY}tab: switch  enter: launch  ctrl+r: reset config{RESET}"
        elif self.state.launch.current_field == LaunchField.OPEN_EDITOR:
            cmd, label = self.tui.resolve_editor_command()
            if cmd:
                friendly = label or "VS Code"
                return f"{FG_GRAY}tab: switch  enter: open {friendly}{RESET}"
            return f"{FG_GRAY}tab: switch  (editor not available - install code CLI or set $EDITOR){RESET}"

        # Section headers (cursor == -1)
        elif self.state.launch.current_field == LaunchField.CLAUDE_SECTION and self.state.launch.claude_cursor == -1:
            return f"{FG_GRAY}tab: switch  enter: expand/collapse  ctrl+r: reset config{RESET}"
        elif self.state.launch.current_field == LaunchField.HCOM_SECTION and self.state.launch.hcom_cursor == -1:
            return f"{FG_GRAY}tab: switch  enter: expand/collapse  ctrl+r: reset config{RESET}"
        elif (
            self.state.launch.current_field == LaunchField.CUSTOM_ENV_SECTION
            and self.state.launch.custom_env_cursor == -1
        ):
            return f"{FG_GRAY}tab: switch  enter: expand/collapse  ctrl+r: reset config{RESET}"

        # Fields within sections (cursor >= 0)
        elif self.state.launch.current_field == LaunchField.CLAUDE_SECTION and self.state.launch.claude_cursor >= 0:
            fields = self.build_claude_fields()
            if self.state.launch.claude_cursor < len(fields):
                field = fields[self.state.launch.claude_cursor]
                if field.field_type == "checkbox":
                    return f"{FG_GRAY}tab: switch  enter: toggle  ctrl+r: reset config{RESET}"
                elif field.field_type == "cycle":
                    return f"{FG_GRAY}tab: switch  ←→: cycle options  enter: next  ctrl+r: reset config{RESET}"
                else:  # text fields
                    return f"{FG_GRAY}tab: switch  type: edit  ←→: cursor  esc: clear  ctrl+r: reset config{RESET}"

        elif self.state.launch.current_field == LaunchField.HCOM_SECTION and self.state.launch.hcom_cursor >= 0:
            fields = self.build_hcom_fields()
            if self.state.launch.hcom_cursor < len(fields):
                field = fields[self.state.launch.hcom_cursor]
                if field.field_type == "checkbox":
                    return f"{FG_GRAY}tab: switch  enter: toggle  ctrl+r: reset config{RESET}"
                elif field.field_type == "cycle":
                    # Special handling for HCOM_TERMINAL with custom mode
                    if field.key == "HCOM_TERMINAL":
                        from ..terminal import get_available_presets

                        preset_names = [name for name, _ in get_available_presets()]
                        value = self.state.config_edit.get("HCOM_TERMINAL", "")
                        if value and value not in preset_names:
                            # In custom mode - show text editing hints
                            return f"{FG_GRAY}tab: switch  type: edit  ←→: cursor  esc: back to presets  ctrl+r: reset config{RESET}"
                        elif not value:
                            # Empty custom mode - waiting for input
                            return (
                                f"{FG_GRAY}tab: switch  type: command  ←→: back to presets  ctrl+r: reset config{RESET}"
                            )
                    return f"{FG_GRAY}tab: switch  ←→: cycle options  esc: clear  ctrl+r: reset config{RESET}"
                elif field.field_type == "multi_cycle":
                    return f"{FG_GRAY}←→: cycle options  space: add/remove  esc: clear all{RESET}"
                elif field.field_type == "numeric":
                    return f"{FG_GRAY}tab: switch  type: digits  ←→: cursor  esc: clear  ctrl+r: reset config{RESET}"
                else:  # text fields
                    return f"{FG_GRAY}tab: switch  type: edit  ←→: cursor  esc: clear  ctrl+r: reset config{RESET}"

        elif (
            self.state.launch.current_field == LaunchField.CUSTOM_ENV_SECTION
            and self.state.launch.custom_env_cursor >= 0
        ):
            return f"{FG_GRAY}tab: switch  type: edit  ←→: cursor  esc: clear  ctrl+r: reset config{RESET}"

        # Fallback (should not happen)
        return f"{FG_GRAY}tab: switch  ctrl+r: reset config{RESET}"

    def do_launch(self):
        """Execute launch using full spec integration"""
        # Check for validation errors first
        if self.state.launch.validation_errors:
            error_fields = ", ".join(self.state.launch.validation_errors.keys())
            self.tui.flash_error(f"Fix config errors before launching: {error_fields}", duration=15.0)
            return

        tool = self.state.launch.tool if self.state.launch.tool in self._tool_options else "claude"

        # Check if tool is installed
        if not self._is_tool_installed(tool):
            self.tui.flash_error(f"{tool} CLI not found in PATH")
            return

        # Parse count
        try:
            count = int(self.state.launch.count) if self.state.launch.count else 1
        except ValueError:
            self.tui.flash_error("Invalid count - must be number")
            return

        # Close stale DB connection before launch - ensures fresh max event ID
        # (fixes inode reuse issue on macOS where TUI's connection persists after reset)
        from ..core.db import close_db

        close_db()

        try:
            # Show launching message
            self.tui.flash(f"Launching {count} {tool} instance{'s' if count != 1 else ''}...")
            self.tui.render()  # Force update to show message

            reload_config()
            if tool == "claude":
                # Claude args parsing + system prompt support
                claude_args_str = self.state.config_edit.get("HCOM_CLAUDE_ARGS", "")
                spec = resolve_claude_args(None, claude_args_str if claude_args_str else None)
                if spec.errors:
                    self.tui.flash_error(f"Invalid HCOM_CLAUDE_ARGS: {'; '.join(spec.errors)}")
                    return

                # Update spec with background and prompt
                background = self.state.launch.background if tool == "claude" else False
                spec = spec.update(
                    background=background,
                    prompt=self.state.launch.prompt,
                )

                # Build tokens, filtering out any existing system prompts from clean_tokens
                # (UI state system prompts take precedence)
                tokens = []
                skip_next = False
                for i, token in enumerate(spec.clean_tokens):
                    if skip_next:
                        skip_next = False
                        continue
                    token_lower = token.lower()
                    # Skip system prompt and allowedTools flags and their values
                    if token_lower in (
                        "--system-prompt",
                        "--append-system-prompt",
                        "--allowedtools",
                        "--allowed-tools",
                    ):
                        # Only skip next token if it's not another flag (it's the value)
                        if i + 1 < len(spec.clean_tokens):
                            next_token = spec.clean_tokens[i + 1]
                            if not next_token.startswith("-"):
                                skip_next = True
                        continue
                    # Skip system prompt= and allowedtools= syntax
                    if token_lower.startswith(
                        ("--system-prompt=", "--append-system-prompt=", "--allowedtools=", "--allowed-tools=")
                    ):
                        continue
                    tokens.append(token)

                # Add UI state system prompts
                if self.state.launch.system_prompt:
                    tokens.extend(["--system-prompt", self.state.launch.system_prompt])
                if self.state.launch.append_system_prompt:
                    tokens.extend(
                        [
                            "--append-system-prompt",
                            self.state.launch.append_system_prompt,
                        ]
                    )
                # Add allowed tools if specified
                if self.state.launch.allowed_tools:
                    tokens.extend(["--allowedTools", self.state.launch.allowed_tools])

                spec = resolve_claude_args(tokens, None)
                claude_tokens = spec.rebuild_tokens()

                # PTY mode: use PTY wrapper for interactive Claude (not headless, not Windows)
                use_pty = not background and not IS_WINDOWS
                result = unified_launch(
                    tool,
                    count,
                    claude_tokens,
                    tag=(self.state.config_edit.get("HCOM_TAG", "") or None),
                    background=background,
                    pty=use_pty,
                    run_here=False,  # TUI: always launch in new terminal
                )
            elif tool == "codex":
                # Codex: uses HCOM_CODEX_ARGS from config (checkbox updates field directly)
                # Hook setup moved to launcher.launch() - single source of truth

                # Validate Codex args before launch
                config_args = self.state.config_edit.get("HCOM_CODEX_ARGS", "")
                if config_args:
                    codex_spec = resolve_codex_args(None, config_args)
                    if codex_spec.errors:
                        self.tui.flash_error(f"Invalid HCOM_CODEX_ARGS: {'; '.join(codex_spec.errors)}")
                        return

                # Get config args (checkbox already updated these)
                codex_args = shlex.split(config_args) if config_args else []
                headless = self.state.launch.background

                result = unified_launch(
                    tool,
                    count,
                    codex_args,
                    tag=(self.state.config_edit.get("HCOM_TAG", "") or None),
                    prompt=self.state.launch.codex_prompt or None,
                    system_prompt=self.state.launch.codex_system_prompt or None,
                    background=headless,
                    run_here=False,  # TUI: always launch in new terminal
                )
            else:
                # Gemini: uses HCOM_GEMINI_ARGS from config, prompt injected via PTY or headless.
                # Hook setup + version check moved to launcher.launch() - single source of truth

                # Validate Gemini args before launch
                config_args = self.state.config_edit.get("HCOM_GEMINI_ARGS", "")
                if config_args:
                    gemini_spec = resolve_gemini_args(None, config_args)
                    if gemini_spec.errors:
                        self.tui.flash_error(f"Invalid HCOM_GEMINI_ARGS: {'; '.join(gemini_spec.errors)}")
                        return

                gemini_args = shlex.split(config_args) if config_args else []
                headless = self.state.launch.background
                result = unified_launch(
                    tool,
                    count,
                    gemini_args,
                    tag=(self.state.config_edit.get("HCOM_TAG", "") or None),
                    prompt=self.state.launch.gemini_prompt or None,
                    system_prompt=self.state.launch.gemini_system_prompt or None,
                    background=headless,
                    run_here=False,  # TUI: always launch in new terminal
                )

            if result and result.get("launched", 0) > 0:
                # Switch to Manage screen to see new instances
                self.tui.mode = Mode.MANAGE
                self.tui.flash(f"Launched {result['launched']} instance{'s' if result['launched'] != 1 else ''}")
                self.tui.load_status()  # Refresh immediately
            else:
                # Surface the actual error message from launcher
                errors = result.get("errors", []) if result else []
                if errors:
                    error_msg = errors[0].get("error", "Unknown error")
                    self.tui.flash_error(f"Launch failed: {error_msg}")
                else:
                    self.tui.flash_error("Launch failed - check instances")

        except Exception as e:
            # cmd_launch raises CLIError for validation failures
            self.tui.flash_error(str(e))
        finally:
            pass
