"""Admin commands for HCOM - re-exports from focused modules for backward compatibility"""

# Re-export all cmd_* functions for backward compatibility
from .events import cmd_events
from .query import cmd_list, cmd_archive, cmd_status
from .relay import cmd_relay, REQUIRED_RELAY_VERSION
from .transcript import cmd_transcript
from .reset import (
    cmd_reset,
    clear,
    remove_global_hooks,
    reset_config,
    get_archive_timestamp,
    cmd_help,
)
from .config_cmd import cmd_config

__all__ = [
    # Events
    "cmd_events",
    # Query
    "cmd_list",
    "cmd_archive",
    "cmd_status",
    # Relay
    "cmd_relay",
    "REQUIRED_RELAY_VERSION",
    # Transcript
    "cmd_transcript",
    # Reset
    "cmd_reset",
    "clear",
    "remove_global_hooks",
    "reset_config",
    "get_archive_timestamp",
    "cmd_help",
    # Config
    "cmd_config",
]
