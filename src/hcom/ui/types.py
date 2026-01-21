"""UI type definitions.

This module defines all state types and enums used by the TUI.

The state is organized hierarchically:
- UIState is the root, containing nested state objects for each screen
- Each screen has its own state dataclass (ManageState, LaunchState, etc.)
- All state is mutable and updated in place during user interaction

State Flow
----------
    User Input → handle_key() → state mutation → render() → display

The state-first approach means screens don't hold internal state; everything
is in the shared UIState object passed to each screen constructor.
"""

from __future__ import annotations
from dataclasses import dataclass, field
from enum import Enum
from typing import List, Optional, Literal


@dataclass
class Field:
    """Configuration field for rendering in expandable sections.

    Used by LaunchScreen to render form fields with appropriate controls.

    Attributes:
        key: Config key name (e.g., "HCOM_TAG", "prompt").
        display_name: Human-readable label shown in the UI.
        field_type: Control type determining interaction behavior:
            - "checkbox": Toggle boolean value
            - "text": Free-form text input
            - "cycle": Arrow keys cycle through options
            - "numeric": Arrow keys increment/decrement
            - "multi_cycle": Cycle + space to add multiple values
        value: Current value (str for most, bool for checkbox).
        options: List of choices for cycle/multi_cycle types.
        hint: Help text shown when field is selected.
    """

    key: str
    display_name: str
    field_type: Literal["checkbox", "text", "cycle", "numeric", "multi_cycle"]
    value: str | bool
    options: List[str] | None = None
    hint: str = ""


class Mode(Enum):
    """TUI screen mode. Tab switches between modes."""

    MANAGE = "manage"  # Instance list + messaging
    LAUNCH = "launch"  # Instance creation form


class LaunchField(Enum):
    """Navigable fields in the Launch screen.

    Used to track cursor position in the form. Values represent
    vertical navigation order.
    """

    TOOL = 0  # Tool selector (claude/gemini/codex)
    COUNT = 1  # Instance count
    LAUNCH_BTN = 2  # Launch button
    CLAUDE_SECTION = 3  # Tool-specific config section
    HCOM_SECTION = 4  # HCOM config section
    CUSTOM_ENV_SECTION = 5  # Custom environment variables
    OPEN_EDITOR = 6  # Open config.env in editor


# ===== Nested State Dataclasses =====


@dataclass
class ManageState:
    """State for the Manage screen (instance list + messaging).

    Tracks cursor position, displayed instances, messages, and input buffer.

    Attributes:
        cursor: Current row position in the instance list (0-indexed).
        cursor_instance_name: Name of instance at cursor (for stable positioning).
        instances: Dict of {display_name: instance_info} for rendering.
        status_counts: Dict of {status: count} for status bar.
        messages: List of (time, sender, text, delivered_to, event_id) tuples.
        message_buffer: Current text being composed in input area.
        message_cursor_pos: Character position in message_buffer.
        instance_scroll_pos: Scroll offset for long instance lists.
        show_instance_detail: Instance name to show detail panel for (or None).
        show_stopped: Whether to show stopped instances section.
        show_remote: Whether to expand remote instances section.
        show_stopped_user_set: True if user manually toggled stopped visibility.
        show_remote_user_set: True if user manually toggled remote visibility.
        device_sync_times: Dict of {device_id: last_sync_timestamp} for relay.
        send_state: Message send animation state (None, "sending", "sent").
        send_state_until: Timestamp when send_state should clear.
        unread_counts: Dict of {instance_name: unread_count} for indicators.
    """

    cursor: int = 0
    cursor_instance_name: Optional[str] = None
    instances: dict = field(default_factory=dict)
    status_counts: dict = field(default_factory=dict)
    messages: list = field(default_factory=list)
    message_buffer: str = ""
    message_cursor_pos: int = 0
    instance_scroll_pos: int = 0
    show_instance_detail: Optional[str] = None
    show_stopped: bool = False
    show_remote: bool = False
    show_stopped_user_set: bool = False
    show_remote_user_set: bool = False
    device_sync_times: dict = field(default_factory=dict)
    send_state: Optional[str] = None  # None, 'sending', 'sent'
    send_state_until: float = 0.0
    unread_counts: dict = field(default_factory=dict)


@dataclass
class LaunchState:
    """State for the Launch screen (instance creation form).

    Contains form values for launching new instances. Each tool (Claude, Gemini,
    Codex) has its own prompt/system prompt fields since they persist independently.

    Attributes:
        tool: Selected tool type ("claude", "gemini", "codex").
        count: Number of instances to launch (as string for text input).
        prompt: Claude initial prompt text.
        prompt_cursor: Cursor position in prompt field.
        system_prompt: Claude system prompt (--system-prompt flag).
        system_prompt_cursor: Cursor position in system_prompt field.
        append_system_prompt: Text to append to default system prompt.
        append_system_prompt_cursor: Cursor position in append field.
        allowed_tools: Comma-separated Claude Code allowed tools.
        background: Launch in headless mode (no PTY, hooks only).
        gemini_prompt: Gemini initial prompt text (-i flag).
        gemini_prompt_cursor: Cursor position in Gemini prompt.
        gemini_system_prompt: Gemini system prompt (GEMINI_SYSTEM_MD).
        gemini_system_prompt_cursor: Cursor position in Gemini system prompt.
        codex_prompt: Codex initial prompt text.
        codex_prompt_cursor: Cursor position in Codex prompt.
        codex_system_prompt: Codex system prompt (developer_instructions).
        codex_system_prompt_cursor: Cursor position in Codex system prompt.
        codex_sandbox_mode: Codex sandbox mode (workspace/untrusted/etc).
        current_field: Currently selected form field (LaunchField enum).
        scroll_pos: Vertical scroll offset for long forms.
        claude_expanded: Whether tool section is expanded.
        hcom_expanded: Whether HCOM config section is expanded.
        custom_env_expanded: Whether custom env section is expanded.
        claude_cursor: Position within tool section (-1 = header).
        hcom_cursor: Position within HCOM section (-1 = header).
        custom_env_cursor: Position within custom env section (-1 = header).
        config_field_cursors: Dict of {field_key: cursor_position} for text fields.
        multi_cycle_indices: Dict of {field_key: current_option_index} for cycles.
        validation_errors: Dict of {field_key: error_message} for invalid values.
    """

    tool: str = "claude"
    count: str = "1"
    prompt: str = ""
    prompt_cursor: int = 0
    system_prompt: str = ""
    system_prompt_cursor: int = 0
    append_system_prompt: str = ""
    append_system_prompt_cursor: int = 0
    allowed_tools: str = ""  # Comma-separated tools for --allowedTools
    background: bool = False
    # Gemini-specific
    gemini_prompt: str = ""
    gemini_prompt_cursor: int = 0
    gemini_system_prompt: str = ""
    gemini_system_prompt_cursor: int = 0
    # Codex-specific
    codex_prompt: str = ""
    codex_prompt_cursor: int = 0
    codex_system_prompt: str = ""
    codex_system_prompt_cursor: int = 0
    codex_sandbox_mode: str = "workspace"
    # Navigation
    current_field: LaunchField = LaunchField.COUNT
    scroll_pos: int = 0
    # Section expansion
    claude_expanded: bool = False
    hcom_expanded: bool = False
    custom_env_expanded: bool = False
    claude_cursor: int = -1
    hcom_cursor: int = -1
    custom_env_cursor: int = -1
    # Field cursors and validation
    config_field_cursors: dict = field(default_factory=dict)
    multi_cycle_indices: dict = field(default_factory=dict)
    validation_errors: dict = field(default_factory=dict)


@dataclass
class EventsState:
    """State for the Events view.

    Tracks filter settings and archive navigation for event display.

    Attributes:
        filter: SQL filter expression for events query.
        filter_cursor: Cursor position in filter input.
        type_filter: Event type filter ("all", "message", "status", "life").
        instances_view: Whether showing per-instance view.
        instances_cursor: Cursor position in instances list.
        instances_list: List of instance names for selection.
        instances_data: Cached instance data for rendering.
        archive_mode: Whether viewing archived sessions.
        archive_index: Selected archive session index.
        archive_picker: Whether archive picker is open.
        archive_list: List of available archive sessions.
        archive_cursor: Cursor position in archive picker.
        last_event_id: Highest event ID seen (for incremental loading).
    """

    filter: str = ""
    filter_cursor: int = 0
    type_filter: str = "all"  # "all", "message", "status", "life"
    instances_view: bool = False
    instances_cursor: int = 0
    instances_list: list = field(default_factory=list)
    instances_data: list = field(default_factory=list)
    # Archive
    archive_mode: bool = False
    archive_index: int = 0
    archive_picker: bool = False
    archive_list: list = field(default_factory=list)
    archive_cursor: int = 0
    # Cache
    last_event_id: int = 0


@dataclass
class ConfirmState:
    """State for two-step confirmation dialogs.

    Destructive actions require pressing the same key twice within a timeout.
    This prevents accidental data loss.

    Attributes:
        pending_stop: Instance name awaiting stop confirmation (or None).
        pending_stop_time: Timestamp when stop confirmation was requested.
        pending_stop_all: Whether stop-all confirmation is pending.
        pending_stop_all_time: Timestamp when stop-all was requested.
        pending_reset: Whether reset confirmation is pending.
        pending_reset_time: Timestamp when reset was requested.
    """

    pending_stop: Optional[str] = None
    pending_stop_time: float = 0.0
    pending_stop_all: bool = False
    pending_stop_all_time: float = 0.0
    pending_reset: bool = False
    pending_reset_time: float = 0.0


@dataclass
class RelayState:
    """State for relay sync status.

    Tracks cross-device sync configuration and last sync result.

    Attributes:
        configured: Whether relay URL is configured.
        enabled: Whether relay sync is enabled.
        status: Last sync result ("ok", "error", or None if never synced).
        error: Error message from last failed sync (or None).
    """

    configured: bool = False
    enabled: bool = True
    status: Optional[str] = None  # 'ok' | 'error' | None
    error: Optional[str] = None


@dataclass
class UIState:
    """Root state object shared by all TUI screens.

    This is the single source of truth for UI state. All screens receive
    a reference to this object and mutate it directly.

    Attributes:
        manage: State for the Manage screen (instances, messages).
        launch: State for the Launch screen (form values).
        events: State for the Events view (filters, archive).
        confirm: State for two-step confirmation dialogs.
        relay: State for cross-device relay sync.
        config_edit: Dict of config values being edited (key -> value).
        config_mtime: Last modification time of config.env file.
        frame_dirty: Whether screen needs re-render (optimization).
        flash_message: Current flash notification text (or None).
        flash_until: Timestamp when flash should disappear.
        flash_color: Flash background color ("orange", "red", "white").
        last_message_time: Timestamp of most recent message (for pulse effect).
        archive_count: Number of archived sessions (for empty state display).
    """

    # Nested state groups
    manage: ManageState = field(default_factory=ManageState)
    launch: LaunchState = field(default_factory=LaunchState)
    events: EventsState = field(default_factory=EventsState)
    confirm: ConfirmState = field(default_factory=ConfirmState)
    relay: RelayState = field(default_factory=RelayState)

    # Shared config state
    config_edit: dict = field(default_factory=dict)
    config_mtime: float = 0.0

    # Rendering
    frame_dirty: bool = True

    # Flash notifications
    flash_message: Optional[str] = None
    flash_until: float = 0.0
    flash_color: str = "orange"

    # Cache
    last_message_time: float = 0.0
    archive_count: int = 0
