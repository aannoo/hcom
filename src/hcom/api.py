"""Public API for TUI and external tools

Simple re-export module providing consistent import point.
Not a service layer - just consolidates public surface.

CLI and internal code can import from specific modules.
TUI and plugins should import from api.py for stability.
"""
from __future__ import annotations

# Core utilities
from .core.config import (
    get_config,
    reload_config,
    HcomConfig,
    HcomConfigError,
    ConfigSnapshot,
    load_config_snapshot,
    save_config_snapshot,
    save_config,
    dict_to_hcom_config,
)
from .core.paths import hcom_path, ensure_hcom_directories
from .core.instances import (
    get_instance_status,
    set_status,
    load_instance_position,
    update_instance_position,
)
from .core.messages import (
    send_message,
    get_unread_messages,
    get_read_receipts,
)

# Commands (for TUI to call directly)
from .commands.admin import (
    cmd_reset,
    cmd_events,
)
from .commands.lifecycle import (
    cmd_launch,
    cmd_start,
    cmd_stop,
)
from .commands.messaging import cmd_send

# Shared utilities and constants
from .shared import (
    ClaudeArgsSpec,
    resolve_claude_args,
)

__all__ = [
    # Config
    'get_config',
    'reload_config',
    'HcomConfig',
    'HcomConfigError',
    'ConfigSnapshot',
    'load_config_snapshot',
    'save_config_snapshot',
    'save_config',
    'dict_to_hcom_config',
    # Paths
    'hcom_path',
    'ensure_hcom_directories',
    # Instances
    'get_instance_status',
    'set_status',
    'load_instance_position',
    'update_instance_position',
    # Messages
    'send_message',
    'get_unread_messages',
    'get_read_receipts',
    # Commands
    'cmd_launch',
    'cmd_start',
    'cmd_stop',
    'cmd_send',
    'cmd_reset',
    'cmd_events',
    # Shared
    'ClaudeArgsSpec',
    'resolve_claude_args',
]
