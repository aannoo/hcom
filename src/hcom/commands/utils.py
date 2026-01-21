"""Command utilities for HCOM"""

import sys
from typing import Callable

from ..shared import __version__, is_inside_ai_tool

# Re-export resolve_identity from core.identity (centralized identity resolution)
from ..core.identity import resolve_identity  # noqa: F401


class CLIError(Exception):
    """Raised when arguments cannot be mapped to command semantics."""


def parse_flag_value(
    argv: list[str], flag: str, *, required: bool = True
) -> tuple[str | None, list[str]]:
    """Extract flag value from argv, returning (value, remaining_argv).

    Args:
        argv: Command line arguments (will not be mutated)
        flag: Flag to look for (e.g., "--timeout")
        required: If True, raise CLIError when flag present but value missing

    Returns:
        (value, remaining_argv): Value if flag found, None otherwise.
        remaining_argv has the flag and value removed.

    Raises:
        CLIError: If flag present but value missing (when required=True)
    """
    if flag not in argv:
        return None, argv

    argv = argv.copy()
    idx = argv.index(flag)
    if idx + 1 >= len(argv) or argv[idx + 1].startswith("-"):
        if required:
            raise CLIError(f"{flag} requires a value")
        del argv[idx]
        return None, argv

    value = argv[idx + 1]
    del argv[idx : idx + 2]
    return value, argv


def parse_flag_bool(argv: list[str], flag: str) -> tuple[bool, list[str]]:
    """Extract boolean flag from argv, returning (present, remaining_argv).

    Args:
        argv: Command line arguments (will not be mutated)
        flag: Flag to look for (e.g., "--json")

    Returns:
        (present, remaining_argv): True if flag found, False otherwise.
    """
    if flag not in argv:
        return False, argv
    argv = argv.copy()
    argv.remove(flag)
    return True, argv


# Type for help entries: static tuple or callable returning tuple
HelpEntry = tuple[str, str] | Callable[[], tuple[str, str]]


def _dynamic_terminal_help(tool: str) -> Callable[[], tuple[str, str]]:
    """Create dynamic help entry for tool launch terminal behavior."""

    def _help() -> tuple[str, str]:
        if is_inside_ai_tool():
            return (f"  hcom {tool}", "Opens new terminal")
        return (f"  hcom {tool}", "Runs in current terminal")

    return _help


# Command registry - single source of truth for CLI help
# Format: list of (usage, description) tuples per command
# Entries can be static tuples or callables for dynamic content
COMMAND_HELP: dict[str, list[HelpEntry]] = {
    "events": [
        (
            "",
            "Query the event stream (messages, status changes, file edits, lifecycle)",
        ),
        ("", ""),
        ("Query:", ""),
        ("  events", "Recent events as JSON"),
        ("  --last N", "Limit count (default: 20)"),
        ("  --all", "Include archived sessions"),
        ("  --wait [SEC]", "Block until match (default: 60s)"),
        ("  --sql EXPR", "SQL WHERE filter"),
        ("", ""),
        ("Filters (compose with AND):", ""),
        ("  --agent NAME", "Match specific agent"),
        ("  --type TYPE", "message | status | life"),
        ("  --status VAL", "listening | active | blocked"),
        ("  --context PATTERN", "tool:Bash | deliver:X (supports * wildcard)"),
        ("  --file PATH", "File path (*.py for glob, file.py for contains)"),
        ("  --cmd PATTERN", "Command (contains default, ^start, =exact)"),
        ("  --from NAME", "Message sender"),
        ("  --mention NAME", "Message mentions"),
        ("  --action VAL", "created | stopped | ready"),
        ("  --intent VAL", "request | inform | ack | error"),
        ("  --thread NAME", "Message thread"),
        ("  --after TIME", "Events after timestamp (ISO-8601)"),
        ("  --before TIME", "Events before timestamp (ISO-8601)"),
        ("  --collision", "File collision detection"),
        ("", ""),
        ("Shortcuts:", ""),
        ("  --idle NAME", "--agent NAME --status listening"),
        ("  --blocked NAME", "--agent NAME --status blocked"),
        ("", ""),
        ("Subscribe:", ""),
        ("  events sub", "List subscriptions"),
        ("  events sub [filters]", "Push notification when event matches"),
        ("    --once", "Auto-remove after first match"),
        ("    --for <name>", "Subscribe for another agent"),
        ("  events unsub <id>", "Remove subscription"),
        ("", ""),
        ("Examples:", ""),
        ("  events --agent peso --status listening", ""),
        ("  events --cmd git --agent peso", ""),
        ("  events --file '*.py' --last 100", ""),
        ("  events sub --idle peso", ""),
        ("  events sub --cmd 'npm test' --once", ""),
        ("", ""),
        ("SQL columns (events_v view):", ""),
        ("  Base", "id, timestamp, type, instance"),
        (
            "  msg_*",
            "from, text, scope, sender_kind, delivered_to[], mentions[], intent, thread, reply_to",
        ),
        ("  status_*", "val, context, detail"),
        ("  life_*", "action, by, batch_id, reason"),
        ("Field values:", ""),
        ("  type", "message, status, life"),
        ("  msg_scope", "broadcast, mentions"),
        ("  msg_sender_kind", "instance, external, system"),
        ("  status_context", "tool:X, deliver:X, approval, prompt, exit:X"),
        ("  life_action", "created, ready, stopped, batch_launched"),
        ("", ""),
        ("", "Example SQL: msg_from = 'luna' AND type = 'message'"),
        ("", "Use <> instead of != for SQL negation"),
    ],
    "list": [
        ("list", "All agents"),
        ("  -v", "Verbose"),
        ("  --json", "Verbose JSON (one per line)"),
        ("", ""),
        ("list [self|<name>]", "Details"),
        ("  [field]", "Print specific field (status, directory, session_id, etc)"),
        ("  --json", "Output as JSON"),
        ("  --sh", 'Shell exports: eval "$(hcom list self --sh)"'),
    ],
    "send": [
        ('send "msg"', "Broadcast message to all your best buddies"),
        ('send "@name msg"', "Send to specific agent/group"),
        ("send --stdin", "Read message from stdin"),
        ("  --name <name>", "Identity (agent name or UUID)"),
        ("  --from <name>", "External sender identity, alias: -b"),
        ("  --bundle JSON", "Create bundle and attach bundle_id to message"),
        ("  --bundle-file F", "Create bundle from JSON file and attach"),
        ("Envelope (optional):", ""),
        ("  --intent <type>", "request|inform|ack|error"),
        ("  --reply-to <id>", "Link to event (42 or 42:BOXE for remote)"),
        ("  --thread <name>", "Group related messages"),
    ],
    "bundle": [
        ("bundle", "List recent bundles"),
        ("bundle list", "List recent bundles"),
        ("  --last N", "Limit count (default: 20)"),
        ("  --json", "Output JSON"),
        ("", ""),
        ("bundle show <id>", "Show bundle by id/prefix"),
        ("  --json", "Output JSON"),
        ("", ""),
        ("bundle create \"title\"", "Create bundle"),
        ("  --description <text>", "Bundle description"),
        ("  --events 1,2", "Comma-separated event IDs"),
        ("  --files a.py,b.py", "Comma-separated file paths"),
        ("  --transcript 4-15,18-22", "Transcript ranges (comma-separated)"),
        ("  --extends <id>", "Parent bundle"),
        ("  --bundle JSON", "Create from JSON payload"),
        ("  --bundle-file F", "Create from JSON file"),
        ("  --json", "Output JSON"),
        ("", ""),
        ("bundle chain <id>", "Show bundle lineage"),
        ("  --json", "Output JSON"),
    ],
    "stop": [
        ("stop", "End hcom participation"),
        ("stop <name>", "End hcom for <name>"),
        ("stop <n1> <n2> ...", "End hcom for multiple"),
        ("stop tag:<name>", "End hcom for all with tag"),
        ("stop all", "End hcom for all"),
    ],
    # NOTE: README references `hcom start -h` for remote/sandbox setup instructions.
    # The sandbox tip below is intentional - agents see it when running help.
    "start": [
        ("", "hcom is a cli tool for multi agent communication"),
        ("", "the hcom start command shows you more info about hcom"),
        ("", ""),
        ("start", "Connect to hcom with new identity"),
        ("start --as <name>", "Reclaim identity after compaction/resume"),
        ("", ""),
        (
            "",
            "Inside a sandbox? Put this in front of every hcom command you run: HCOM_DIR=$PWD/.hcom",
        ),
    ],
    "kill": [
        ("kill <name>", "Kill headless process (Unix only)"),
        ("kill all", "Kill all with tracked PIDs"),
        ("", "Sends SIGTERM to the process group"),
    ],
    "listen": [
        ("listen --name X [timeout]", "Block and receive messages"),
        ("  [timeout]", "Timeout in seconds (alias for --timeout)"),
        ("  --timeout N", "Timeout in seconds (default: 86400)"),
        ("  --json", "Output messages as JSON"),
        ('  --sql "filter"', "Wait for event matching SQL (uses temp subscription)"),
        ("", ""),
        ("SQL filter mode:", ""),
        ("  --sql \"type='message'\"", "Custom SQL against events_v"),
        ("  --idle NAME", "Wait for instance to go idle"),
        ("  --sql EXPR", "SQL WHERE filter"),
        ("  --sql stopped:name", "Preset: wait for instance to stop"),
        ("  --sql blocked:name", "Preset: wait for instance to block"),
        ("", ""),
        ("Exit codes:", ""),
        ("  0", "Message received / event matched"),
        ("  1", "Timeout or error"),
    ],
    "reset": [
        ("reset", "Clear database (archive conversation)"),
        ("reset all", "Stop all + clear db + remove hooks + reset config"),
        ("", ""),
        ("Sandbox/Local Mode:", ""),
        ("  If you can't write to ~/.hcom, set:", ""),
        ('    export HCOM_DIR="$PWD/.hcom"', ""),
        (
            "  This installs hooks under $PWD (.claude/.gemini/.codex) and stores state in $HCOM_DIR",
            "",
        ),
        ("", ""),
        ("  To remove local setup:", ""),
        ('    hcom hooks remove && rm -rf "$HCOM_DIR"', ""),
        ("", ""),
        ("  To use explicit location:", ""),
        ("    export HCOM_DIR=/your/path/.hcom", ""),
        ("", ""),
        ("  To regain global access:", ""),
        ("    Fix ~/.hcom permissions, then: hcom hooks remove", ""),
    ],
    "config": [
        ("config", "Show all config values"),
        ("config <key>", "Get single config value"),
        ("config <key> <val>", "Set config value"),
        ("config <key> --info", "Detailed help for a setting (presets, examples)"),
        ("  --json", "JSON output"),
        ("  --edit", "Open config in $EDITOR"),
        ("  --reset", "Reset config to defaults"),
        ("Runtime agent config:", ""),
        ("config -i <name>", "Show agent config"),
        ("config -i <name> <key>", "Get agent config value"),
        ("config -i <name> <key> <val>", "Set agent config value"),
        ("  -i self", "Current agent (requires Claude/Gemini/Codex context)"),
        ("  keys: tag, timeout, hints, subagent_timeout", ""),
        ("Global settings:", ""),
        ("  HCOM_TAG", "Group tag (creates tag-* names for agents)"),
        ("  HCOM_TERMINAL", 'default | <preset> | "cmd {script}"'),
        ("  HCOM_HINTS", "Text appended to all messages received by agent"),
        ("  HCOM_SUBAGENT_TIMEOUT", "Claude subagent timeout in seconds (default: 30)"),
        ("  HCOM_CLAUDE_ARGS", 'Default claude args (e.g. "--model opus")'),
        ("  HCOM_GEMINI_ARGS", "Default gemini args"),
        ("  HCOM_CODEX_ARGS", "Default codex args"),
        ("  HCOM_RELAY", "Relay server URL (set by 'hcom relay hf')"),
        ("  HCOM_RELAY_TOKEN", "HuggingFace token (set by 'hcom relay hf')"),
        ("  HCOM_AUTO_APPROVE", "Auto-approve safe hcom commands (1|0)"),
        ("  HCOM_AUTO_SUBSCRIBE", 'Auto-subscribe presets (e.g. "collision")'),
        ("  HCOM_NAME_EXPORT", "Export instance name to custom env var"),
        ("", ""),
        ("", "Non-HCOM_* vars in config.env pass through to Claude/Gemini/Codex"),
        ("", "e.g. ANTHROPIC_MODEL=opus"),
        ("", ""),
        ("Precedence:", "HCOM defaults < config.env < shell env vars"),
        ("", "Each resolves independently"),
    ],
    "relay": [
        ("relay", "Show relay status"),
        ("relay on", "Enable cross-device live sync"),
        ("relay off", "Disable cross-device live sync"),
        ("relay pull", "Force sync now"),
        ("relay hf [token]", "Setup HuggingFace Space relay"),
        ("  --update", "Update existing Space"),
        (
            "",
            "Finds or duplicates a private, free HF Space to your account as the relay server.",
        ),
        ("", "Provide HF_TOKEN or run 'huggingface-cli login' first."),
    ],
    "transcript": [
        ("transcript @name", "View another agent's conversation"),
        ("transcript @name N", "Show exchange N"),
        ("transcript @name N-M", "Show exchanges N through M"),
        ("transcript timeline", "Follow user prompts across all transcripts by time"),
        ("  --last N", "Limit to last N exchanges (default: 10)"),
        ("  --full", "Show full assistant responses"),
        ("  --detailed", "Show tool I/O, edits, errors"),
        ("  --json", "JSON output"),
        ("", ""),
        ('transcript search "pattern"', "Search hcom-tracked transcripts (rg or grep)"),
        ("  --live", "Only currently alive agents"),
        ("  --all", "All transcripts (includes non-hcom sessions)"),
        ("  --limit N", "Max results (default: 20)"),
        ("  --agent TYPE", "Filter: claude, gemini, or codex"),
        ("  --json", "JSON output"),
    ],
    "archive": [
        ("archive", "List archived sessions (numbered)"),
        ("archive <N>", "Query events from archive (1 = most recent)"),
        ("archive <N> agents", "Query agents from archive"),
        ("archive <name>", "Query by stable name (prefix match works)"),
        ("  --here", "Filter to archives with current directory"),
        ('  --sql "expr"', "SQL WHERE filter"),
        ("  --last N", "Limit to last N events (default: 20)"),
        ("  --json", "JSON output"),
    ],
    "run": [
        ("run", "List available workflow/launch scripts"),
        ("run <name> [args]", "Run script or profile"),
        ("", ""),
        ("", "Run `hcom run` to see available scripts and more info"),
        ("", "Run `hcom run <script> --help` for script options"),
        ("", "Run `hcom run docs` for Python API + full CLI ref + examples"),
        ("", ""),
        ("", "Docs sections:"),
        ("  hcom run docs --cli", "CLI reference only"),
        ("  hcom run docs --config", "Config settings only"),
        ("  hcom run docs --api", "Python API + scripts guide"),
    ],
    "claude": [
        ("[N] claude [args...]", "Launch N Claude agents (default N=1)"),
        ("", ""),
        _dynamic_terminal_help("claude"),
        ("  hcom N claude (N>1)", "Opens new terminal windows"),
        ('  hcom N claude "do task x"', "initial prompt"),
        ('  hcom 3 claude -p "prompt"', "3 headless in background"),
        ("  HCOM_TAG=api hcom 2 claude", "Group tag (creates api-*)"),
        ("  hcom 1 claude --agent <name>", ".claude/agents/<name>.md"),
        ('  hcom 1 claude --system-prompt "text"', "System prompt"),
        ("", ""),
        ("Environment:", ""),
        ("  HCOM_TAG", "Group tag (agents become tag-*)"),
        ("  HCOM_TERMINAL", 'default | <preset> | "cmd {script}"'),
        ("  HCOM_CLAUDE_ARGS", "Default args (merged with CLI)"),
        ("  HCOM_HINTS", "Appended to messages received"),
        (
            "  HCOM_SUBAGENT_TIMEOUT",
            "Seconds claude subagents are kept alive after finishing task",
        ),
        ("", ""),
        ("", 'Run "claude --help" for Claude CLI options'),
    ],
    "gemini": [
        ("[N] gemini [args...]", "Launch N Gemini agents (default N=1)"),
        ("", ""),
        _dynamic_terminal_help("gemini"),
        ("  hcom N gemini (N>1)", "Opens new terminal windows"),
        ('  hcom N gemini "do task x"', "initial regular prompt"),
        ("  hcom N gemini --yolo", "flags forwarded to gemini"),
        ("  HCOM_TAG=api hcom 2 gemini", "Group tag (creates api-*)"),
        ("", ""),
        ("Environment:", ""),
        ("  HCOM_TAG", "Group tag (agents become tag-*)"),
        ("  HCOM_TERMINAL", 'default | <preset> | "cmd {script}"'),
        ("  HCOM_GEMINI_ARGS", "Default args (merged with CLI)"),
        ("  HCOM_HINTS", "Appended to all messages received"),
        ("  HCOM_GEMINI_SYSTEM_PROMPT", "Use this for system prompt"),
        ("", ""),
        ("", 'Run "gemini --help" for Gemini CLI options'),
    ],
    "codex": [
        ("[N] codex [args...]", "Launch N Codex agents (default N=1)"),
        ("", ""),
        _dynamic_terminal_help("codex"),
        ("  hcom N codex (N>1)", "Opens new terminal windows"),
        ('  hcom N codex "do task x"', "initial regular prompt"),
        ("  hcom codex --sandbox danger-full-access", "flags forwarded to codex"),
        ("  HCOM_TAG=api hcom 2 codex", "Group tag (creates api-*)"),
        ("", ""),
        ("Environment:", ""),
        ("  HCOM_TAG", "Group tag (agents become tag-*)"),
        ("  HCOM_TERMINAL", 'default | <preset> | "cmd {script}"'),
        ("  HCOM_CODEX_ARGS", "Default args (merged with CLI)"),
        ("  HCOM_HINTS", "Appended to messages received"),
        ("  HCOM_CODEX_SYSTEM_PROMPT", "Use this for system prompt"),
        ("", ""),
        ("", 'Run "codex --help" for Codex CLI options'),
    ],
    "status": [
        ("status", "Show hcom installation status and diagnostics"),
        ("status --logs", "Include recent errors and warnings"),
        ("status --json", "Machine-readable output"),
    ],
    "hooks": [
        ("hooks", "Show hook status"),
        ("hooks status", "Same as above"),
        ("hooks add [tool]", "Add hooks (claude|gemini|codex|all)"),
        ("hooks remove [tool]", "Remove hooks (claude|gemini|codex|all)"),
        ("", ""),
        ("", "Hooks enable automatic message delivery and status tracking."),
        ("", "Without hooks, use ad-hoc mode (run hcom start in any ai tool)."),
        ("", ""),
        ("", "After adding, restart the tool to activate hooks."),
        ("", "Remove cleans both global (~/) and HCOM_DIR-local if set."),
    ],
}


def get_command_help(name: str) -> str:
    """Get formatted help for a single command."""
    if name not in COMMAND_HELP:
        return f"Usage: hcom {name}"
    lines = ["Usage:"]
    for entry in COMMAND_HELP[name]:
        # Handle callable entries (dynamic content)
        usage, desc = entry() if callable(entry) else entry
        if not usage:  # Empty line or plain text
            lines.append(f"  {desc}" if desc else "")
        elif usage.startswith("  "):  # Option/setting line (indented)
            lines.append(f"  {usage:<32} {desc}")
        elif usage.endswith(":"):  # Section header
            lines.append(f"\n{usage} {desc}" if desc else f"\n{usage}")
        else:  # Command line
            lines.append(f"  hcom {usage:<26} {desc}")
    return "\n".join(lines)


def get_help_text() -> str:
    """Generate help text with current version"""
    return f"""hcom (hook-comms) v{__version__} - multi-agent communication

Usage:
  hcom                                  TUI dashboard
  hcom <N> claude|gemini|codex [args]   Launch (args passed to tool)
  hcom <command>                        Run command

Commands:
  send         Send message to your buddies
  listen       Block and receive messages
  bundle       Create and query bundles
  list         Show participants, status, read receipts
  start        Enable hcom participation
  stop         Disable hcom participation
  events       Query events / subscribe for push notifications
  transcript   View another agent's conversation
  run          Run workflows from ~/.hcom/scripts/
  config       Get/set config environment variables
  relay        Cross-device live chat
  archive      Query archived sessions
  reset        Archive & clear database
  hooks        Add or remove hooks
  status       Show installation status and diagnostics

Identity:
  1. Run hcom start to get name
  2. Use --name in all the other hcom commands

Run 'hcom <command> --help' for details.
"""


# Known flags per command - for validation against hallucinated flags
# Global flags accepted by all commands: identity (--name) and help (--help, -h)
_GLOBAL_FLAGS = {"--name", "--help", "-h"}

# Composable filter flags (used by events, events sub, listen)
_FILTER_FLAGS = {
    "--agent",
    "--type",
    "--status",
    "--context",
    "--file",
    "--cmd",
    "--from",
    "--mention",
    "--action",
    "--after",
    "--before",
    "--intent",
    "--thread",
    "--reply-to",
    "--idle",
    "--blocked",
    "--collision",
}

KNOWN_FLAGS: dict[str, set[str]] = {
    "send": _GLOBAL_FLAGS
    | {
        "--intent",
        "--reply-to",
        "--thread",
        "--stdin",
        "--from",
        "-b",
        "--bundle",
        "--bundle-file",
    },
    "events": _GLOBAL_FLAGS | _FILTER_FLAGS | {"--last", "--wait", "--sql", "--all"},
    "events sub": _GLOBAL_FLAGS | _FILTER_FLAGS | {"--once", "--for"},
    "events unsub": _GLOBAL_FLAGS,
    "events launch": _GLOBAL_FLAGS,
    "list": _GLOBAL_FLAGS | {"--json", "-v", "--verbose", "--sh"},
    "listen": _GLOBAL_FLAGS | _FILTER_FLAGS | {"--timeout", "--json", "--sql"},
    "start": _GLOBAL_FLAGS | {"--as"},
    "kill": _GLOBAL_FLAGS,
    "stop": _GLOBAL_FLAGS,
    "transcript": _GLOBAL_FLAGS
    | {"--last", "--range", "--json", "--full", "--detailed"},
    "transcript timeline": _GLOBAL_FLAGS | {"--last", "--json", "--full", "--detailed"},
    "transcript search": _GLOBAL_FLAGS | {"--limit", "--json", "--agent", "--live", "--all"},
    "config": _GLOBAL_FLAGS | {"--json", "--edit", "--reset", "-i", "--info"},
    "reset": _GLOBAL_FLAGS,
    "relay": _GLOBAL_FLAGS | {"--space", "--update"},
    "archive": _GLOBAL_FLAGS | {"--json", "--here", "--sql", "--last"},
    "status": _GLOBAL_FLAGS | {"--json", "--logs"},
    "run": _GLOBAL_FLAGS,
    "hooks": _GLOBAL_FLAGS,
    "bundle": _GLOBAL_FLAGS | {"--json", "--last"},
    "bundle list": _GLOBAL_FLAGS | {"--json", "--last"},
    "bundle show": _GLOBAL_FLAGS | {"--json"},
    "bundle create": _GLOBAL_FLAGS
    | {
        "--json",
        "--title",
        "--description",
        "--events",
        "--files",
        "--transcript",
        "--extends",
        "--bundle",
        "--bundle-file",
    },
    "bundle chain": _GLOBAL_FLAGS | {"--json"},
}


def validate_flags(cmd: str, argv: list[str]) -> str | None:
    """Validate flags against known flags for command.

    Returns error message with help if unknown flag found, None if valid.
    """
    known = KNOWN_FLAGS.get(cmd, set())
    for arg in argv:
        if arg.startswith("-") and arg not in known:
            help_text = get_command_help(cmd)
            return f"Unknown flag '{arg}'\n\n{help_text}"
    return None


def format_error(message: str, suggestion: str | None = None) -> str:
    """Format error message consistently"""
    base = f"Error: {message}"
    if suggestion:
        base += f". {suggestion}"
    return base


def is_interactive() -> bool:
    """Check if running in interactive mode"""
    return sys.stdin.isatty() and sys.stdout.isatty()


def validate_message(message: str) -> str | None:
    """Validate message size and content. Returns formatted error or None if valid."""
    from ..core.messages import validate_message as core_validate

    error = core_validate(message)
    return format_error(error) if error else None


def parse_name_flag(argv: list[str]) -> tuple[str | None, list[str]]:
    """Parse --name flag from argv.

    The --name flag is the identity flag (strict instance lookup).

    Resolution (handled by resolve_from_name in core.identity):
    - Instance name → kind='instance'
    - Agent ID (UUID) → kind='instance'
    - Error if not found (no external fallback)

    Args:
        argv: Command line arguments

    Returns:
        (name_value, remaining_argv): Identity value if flag provided and argv with flag removed.

    Raises:
        CLIError: If --name is provided without a value.
    """
    argv = argv.copy()  # Don't mutate original
    name_value: str | None = None

    name_idxs = [i for i, a in enumerate(argv) if a == "--name"]
    if len(name_idxs) > 1:
        raise CLIError("Multiple --name values provided; use exactly one.")
    if name_idxs:
        idx = name_idxs[0]
        if idx + 1 >= len(argv) or argv[idx + 1].startswith("-"):
            raise CLIError("--name requires a value")
        name_value = argv[idx + 1]
        del argv[idx : idx + 2]

    return name_value, argv


def append_unread_messages(instance_name: str, *, json_output: bool = False) -> None:
    """Check for unread messages and print preview with listen instruction.

    Called at end of commands with --name to notify of pending messages.
    Does NOT mark messages as read - instance must run `hcom listen` to receive.

    Args:
        instance_name: The instance to check messages for
        json_output: If True, skip appending (preserve machine-readable format)
    """
    # Skip for JSON output - would corrupt machine-readable format
    if json_output:
        return

    from ..core.messages import get_unread_messages
    from ..pty.pty_common import build_listen_instruction

    # Check if messages exist WITHOUT marking as read
    messages, _ = get_unread_messages(instance_name, update_position=False)
    if not messages:
        return

    # Print preview with listen instruction (no status update needed)
    print("\n" + "─" * 40)
    print("[hcom] new message(s)")
    print("─" * 40)
    print(f"\n{build_listen_instruction(instance_name)}")
