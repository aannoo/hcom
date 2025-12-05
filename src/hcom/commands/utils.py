"""Command utilities for HCOM"""
import sys
import re
from ..shared import __version__, MAX_MESSAGE_SIZE, SenderIdentity, SENDER


class CLIError(Exception):
    """Raised when arguments cannot be mapped to command semantics."""


# Command registry - single source of truth for CLI help
# Format: list of (usage, description) tuples per command
COMMAND_HELP: dict[str, list[tuple[str, str]]] = {
    'events': [
        ('events', 'Query recent events (JSON)'),
        ('  --last N', 'Limit to last N events (default: 20)'),
        ('  --wait [SEC]', 'Block until matching event (default: 60s timeout)'),
        ('  --sql EXPR', 'SQL WHERE clause filter'),
    ],
    'list': [
        ('list', 'List current instances status'),
        ('  -v, --verbose', 'Show detailed metadata'),
        ('  --json', 'Emit JSON with detailed data'),
    ],
    'send': [
        ('send "msg"', 'Send message to all instances'),
        ('send "@alias msg"', 'Send to specific instance/group'),
        ('  --from <name>', 'Custom external identity'),
        ('  --wait', 'Block until reply with --from'),
    ],
    'stop': [
        ('stop', 'Stop current instance (from inside Claude)'),
        ('stop <alias>', 'Stop specific instance'),
        ('stop all', 'Stop all instances'),
    ],
    'start': [
        ('start', 'Start current instance (from inside Claude)'),
        ('start <alias>', 'Start specific instance'),
    ],
    'reset': [
        ('reset', 'Clear database (archive conversation)'),
        ('reset hooks', 'Remove hooks only'),
        ('reset all', 'Stop all + clear db + remove hooks + reset config'),
    ],
    'config': [
        ('config', 'Show all config settings'),
        ('config <key>', 'Get single config value'),
        ('config <key> <val>', 'Set config value'),
        ('  --json', 'JSON output'),
        ('  --edit', 'Open config in $EDITOR'),
        ('  --reset', 'Reset config to defaults'),
    ],
    'relay': [
        ('relay', 'Show relay status'),
        ('relay on', 'Enable relay sync'),
        ('relay off', 'Disable relay sync'),
        ('relay pull', 'Manual relay pull'),
        ('relay hf [token]', 'Setup HuggingFace relay'),
    ],
}


def get_command_help(name: str) -> str:
    """Get formatted help for a single command."""
    if name not in COMMAND_HELP:
        return f"Usage: hcom {name}"
    lines = ['Usage:']
    for usage, desc in COMMAND_HELP[name]:
        if usage.startswith('  '):  # Option line
            lines.append(f"  {usage:<20} {desc}")
        else:  # Command line
            lines.append(f"  hcom {usage:<18} {desc}")
    return '\n'.join(lines)


def _format_commands_section() -> str:
    """Generate Commands section from registry."""
    lines = []
    for name, entries in COMMAND_HELP.items():
        for usage, desc in entries:
            if usage.startswith('  '):  # Option
                lines.append(f"  {usage:<18} {desc}")
            else:  # Command
                lines.append(f"  {usage:<18} {desc}")
        lines.append('')  # Blank line between commands
    return '\n'.join(lines).rstrip()


def get_help_text() -> str:
    """Generate help text with current version"""
    return f"""hcom v{__version__} - Hook-based communication for Claude Code instances

Usage: hcom                           # TUI dashboard
       [ENV_VARS] hcom <COUNT> [claude <ARGS>...]
       hcom events [--last N] [--wait SEC] [--sql EXPR]
       hcom list [--json] [-v|--verbose]
       hcom send "message"
       hcom stop [alias|all]
       hcom start [alias]
       hcom reset [hooks|all]

Launch Examples:
  hcom 3             open 3 terminals with claude connected to hcom
  hcom 3 claude -p                                       + headless
  HCOM_TAG=api hcom 3 claude -p               + @-mention group tag
  claude 'run hcom start'        claude code with prompt also works

Commands:
{_format_commands_section()}

Environment Variables:
  HCOM_TAG=name               Group tag (creates name-* instances)
  HCOM_TERMINAL=mode          Terminal: new|here|print|"custom {{script}}"
  HCOM_HINTS=text             Text appended to all messages received by instance
  HCOM_TIMEOUT=secs           Time until disconnected from hcom chat (default: 1800s / 30m)
  HCOM_SUBAGENT_TIMEOUT=secs  Subagent idle timeout (default: 30s)
  HCOM_CLAUDE_ARGS=args       Claude CLI defaults (e.g., '-p --model opus --agent reviewer')

  ANTHROPIC_MODEL=opus # Any env var passed through to Claude Code

  Persist Env Vars in `~/.hcom/config.env` or use `hcom config`
"""


def format_error(message: str, suggestion: str | None = None) -> str:
    """Format error message consistently"""
    base = f"Error: {message}"
    if suggestion:
        base += f". {suggestion}"
    return base


def is_interactive() -> bool:
    """Check if running in interactive mode"""
    return sys.stdin.isatty() and sys.stdout.isatty()


def resolve_identity(subagent_id: str | None = None, custom_from: str | None = None, system_sender: str | None = None) -> SenderIdentity:
    """Resolve identity in CLI/hook context.

    Args:
        subagent_id: Explicit subagent ID (from Task tool context)
        custom_from: Custom display name (--from flag)
        system_sender: System notification sender name (e.g., 'hcom-launcher')

    Returns:
        SenderIdentity with kind, name, and instance_data

    Identity kind:
        - 'external': Custom sender or CLI (--from or bigboss)
        - 'instance': Real instance (Claude Code with session)
        - 'system': System notifications (launcher, watchdog, etc)
    """
    import os
    from ..shared import MAPID
    from ..core.instances import load_instance_position, resolve_instance_name
    from ..core.config import get_config

    # System sender (internal notifications) - always system
    if system_sender:
        return SenderIdentity(kind='system', name=system_sender, instance_data=None)

    # Custom sender (--from) - always external
    if custom_from:
        return SenderIdentity(kind='external', name=custom_from, instance_data=None)

    # Subagent explicit (Task tool)
    if subagent_id:
        data = load_instance_position(subagent_id)
        if not data:
            # This shouldn't happen - cmd_send validates before calling
            raise ValueError(f"Subagent '{subagent_id}' position data missing")
        return SenderIdentity(kind='instance', name=subagent_id, instance_data=data, session_id=data.get('session_id'))

    # CLI context (not in Claude Code)
    if os.environ.get('CLAUDECODE') != '1':
        return SenderIdentity(kind='external', name=SENDER, instance_data=None)

    # Inside Claude: try session_id (Unix only - CLAUDE_ENV_FILE doesn't work on Windows)
    session_id = os.environ.get('HCOM_SESSION_ID')
    if session_id:
        name, data = resolve_instance_name(session_id, get_config().tag)
        # Return instance identity (data may be None if not opted in yet)
        return SenderIdentity(kind='instance', name=name, instance_data=data, session_id=session_id)

    # Try MAPID (Windows fallback - terminal session ID like WT_SESSION)
    if MAPID:
        from ..core.db import get_instance_by_mapid, get_db
        from ..core.instances import resolve_instance_name

        # First try to find existing instance by MAPID
        data = get_instance_by_mapid(MAPID)
        if data:
            return SenderIdentity(kind='instance', name=data['name'], instance_data=data, session_id=data.get('session_id'))

        # No instance for this MAPID - look up session_id from mapping
        # This handles Windows resume in different terminal (MAPID changes but session_id stays same)
        conn = get_db()
        row = conn.execute(
            "SELECT session_id FROM mapid_sessions WHERE mapid = ?",
            (MAPID,)
        ).fetchone()

        if row:
            # Found session_id mapping - use it for consistent naming across terminals
            session_id = row['session_id']
            name, data = resolve_instance_name(session_id, get_config().tag)
            # Return instance identity (data may be None if not opted in yet)
            return SenderIdentity(kind='instance', name=name, instance_data=data, session_id=session_id)

    # No identity available - fail with error directing to --from
    raise ValueError("Cannot resolve identity - use: hcom send --from <yourname> \"message\"")


def validate_message(message: str) -> str | None:
    """Validate message size and content. Returns error message or None if valid."""
    if not message or not message.strip():
        return format_error("Message required")

    # Reject control characters (except \n, \r, \t)
    if re.search(r'[\x00-\x08\x0B-\x0C\x0E-\x1F\u0080-\u009F]', message):
        return format_error("Message contains control characters")

    if len(message) > MAX_MESSAGE_SIZE:
        return format_error(f"Message too large (max {MAX_MESSAGE_SIZE} chars)")

    return None


def parse_agentid_flag(argv: list[str]) -> tuple[str | None, list[str], str | None]:
    """Parse --agentid flag and return (subagent_id, remaining_argv, agent_id_value)

    Looks up instance by agent_id and returns the instance name.

    Returns:
        (subagent_id, argv, agent_id_value): Instance name if found (else None), argv with flag removed, agent_id value if flag was provided (else None).
    """
    if '--agentid' not in argv:
        return None, argv, None

    idx = argv.index('--agentid')
    if idx + 1 >= len(argv):
        return None, argv, None

    agent_id = argv[idx + 1]
    argv = argv[:idx] + argv[idx + 2:]

    # Look up instance by agent_id
    from ..core.db import get_db
    conn = get_db()
    row = conn.execute(
        "SELECT name FROM instances WHERE agent_id = ?",
        (agent_id,)
    ).fetchone()

    return (row['name'] if row else None), argv, agent_id
