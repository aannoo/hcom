"""Command implementations for HCOM"""

from .lifecycle import cmd_launch, cmd_stop, cmd_start, cmd_kill, cmd_daemon
from .messaging import cmd_send, cmd_listen
from .events import cmd_events
from .reset import cmd_reset, cmd_help
from .query import cmd_list, cmd_archive, cmd_status
from .relay import cmd_relay
from .config_cmd import cmd_config
from .transcript import cmd_transcript
from .shim import cmd_shim
from .hooks_cmd import cmd_hooks
from .bundle import cmd_bundle
from .term import cmd_term
from .utils import CLIError, format_error

__all__ = [
    "cmd_launch",
    "cmd_stop",
    "cmd_start",
    "cmd_kill",
    "cmd_daemon",
    "cmd_send",
    "cmd_listen",
    "cmd_events",
    "cmd_reset",
    "cmd_help",
    "cmd_list",
    "cmd_relay",
    "cmd_config",
    "cmd_transcript",
    "cmd_archive",
    "cmd_status",
    "cmd_shim",
    "cmd_hooks",
    "cmd_bundle",
    "cmd_term",
    "CLIError",
    "format_error",
]
