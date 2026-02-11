"""Command utilities for HCOM"""

import sys
from typing import Callable

from ..shared import __version__, is_inside_ai_tool

# Re-export resolve_identity from core.identity (centralized identity resolution)
from ..core.identity import resolve_identity  # noqa: F401


class CLIError(Exception):
    """Raised when arguments cannot be mapped to command semantics."""


def parse_flag_value(argv: list[str], flag: str, *, required: bool = True) -> tuple[str | None, list[str]]:
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


def parse_last_flag(argv: list[str], default: int = 20) -> tuple[int, list[str]]:
    """Extract --last N flag from argv, returning (value, remaining_argv).

    Args:
        argv: Command line arguments (will not be mutated)
        default: Default value if --last not present

    Returns:
        (value, remaining_argv): The limit value and argv with flag removed.

    Raises:
        CLIError: If --last is present but value is missing or not an integer.
    """
    if "--last" not in argv:
        return default, argv

    argv = argv.copy()
    idx = argv.index("--last")
    if idx + 1 >= len(argv) or argv[idx + 1].startswith("-"):
        raise CLIError("--last requires a number (e.g., --last 20)")
    try:
        value = int(argv[idx + 1])
    except ValueError:
        raise CLIError(f"--last must be an integer, got '{argv[idx + 1]}'")
    del argv[idx : idx + 2]
    return value, argv


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
        ("  events", "Last 20 events as JSON"),
        ("  --last N", "Limit count (default: 20)"),
        ("  --all", "Include archived sessions"),
        ("  --wait [SEC]", "Block until match (default: 60s)"),
        ("  --sql EXPR", "Raw SQL WHERE (ANDed with flags)"),
        ("", ""),
        ("Filters (same flag repeated = OR, different flags = AND):", ""),
        ("", ""),
        ("  Core:", ""),
        ("  --agent NAME", "Agent name"),
        ("  --type TYPE", "message | status | life"),
        ("  --status VAL", "listening | active | blocked"),
        ("  --context PATTERN", "tool:Bash | deliver:X (supports * wildcard)"),
        ("  --action VAL", "created | started | ready | stopped | batch_launched"),
        ("", ""),
        ("  Command / file:", ""),
        ("  --cmd PATTERN", "Shell command (contains, ^prefix, $suffix, =exact, *glob)"),
        ("  --file PATH", "File write (*.py for glob, file.py for contains)"),
        ("  --collision", "Two agents edit same file within 20s"),
        ("", ""),
        ("  Message:", ""),
        ("  --from NAME", "Sender"),
        ("  --mention NAME", "@mention target"),
        ("  --intent VAL", "request | inform | ack"),
        ("  --thread NAME", "Thread name"),
        ("", ""),
        ("  Time:", ""),
        ("  --after TIME", "After timestamp (ISO-8601)"),
        ("  --before TIME", "Before timestamp (ISO-8601)"),
        ("", ""),
        ("Shortcuts:", ""),
        ("  --idle NAME", "--agent NAME --status listening"),
        ("  --blocked NAME", "--agent NAME --status blocked"),
        ("", ""),
        ("Subscribe (hcom notification when event matches):", ""),
        ("  events sub", "List subscriptions"),
        ("  events sub [filters]", "Create subscription with filter flags"),
        ("    --once", "Auto-remove after first match"),
        ("    --for <name>", "Subscribe for another agent"),
        ("  events unsub <id>", "Remove subscription"),
        ("", ""),
        ("Examples:", ""),
        ("  events --agent peso --status listening", ""),
        ("  events --cmd git --agent peso", ""),
        ("", ""),
        ("SQL reference (events_v view):", ""),
        ("  Base", "id, timestamp, type, instance"),
        (
            "  msg_*",
            "from, text, scope, sender_kind, delivered_to[], mentions[], intent, thread, reply_to",
        ),
        ("  status_*", "val, context, detail"),
        ("  life_*", "action, by, batch_id, reason"),
        ("", ""),
        ("  type", "message, status, life"),
        ("  msg_scope", "broadcast, mentions"),
        ("  msg_sender_kind", "instance, external, system"),
        ("  status_context", "tool:X, deliver:X, approval, prompt, exit:X"),
        ("  life_action", "created, ready, stopped, batch_launched"),
        ("", ""),
        ("", "delivered_to/mentions are JSON arrays — use LIKE '%name%' not = 'name'"),
        ("", "Use <> instead of != for SQL negation"),
    ],
    "list": [
        ("list", "All alive agents, read receipts"),
        ("  -v", "Verbose (directory, session, etc)"),
        ("  --json", "Verbose JSON (NDJSON, one per line)"),
        ("", ""),
        ("list [self|<name>]", "Single agent details"),
        ("  [field]", "Print specific field (status, directory, session_id, ...)"),
        ("  --json", "Output as JSON"),
        ("  --sh", 'Shell exports: eval "$(hcom list self --sh)"'),
        ("", ""),
        ("list --stopped [name]", "Stopped instances (from events)"),
        ("  --all", "All stopped (default: last 20)"),
        ("", ""),
        ("Status icons:", ""),
        ("", "▶  active      processing, reads messages very soon"),
        ("", "◉  listening   idle, reads messages in <1s"),
        ("", "■  blocked     needs human approval"),
        ("", "○  inactive    dead or stale"),
        ("", "◦  unknown     neutral"),
        ("", ""),
        ("Tool labels:", ""),
        ("", "[CLAUDE] [GEMINI] [CODEX]  hcom-launched (PTY + hooks)"),
        ("", "[claude] [gemini] [codex]  vanilla (hooks only)"),
        ("", "[AD-HOC]                   manual polling"),
    ],
    "send": [
        ("Usage:", ""),
        ("  send @name -- message text", "Direct message"),
        ("  send @name1 @name2 -- message", "Multiple targets"),
        ("  send -- message text", "Broadcast to all"),
        ("  send @name", "Message from stdin (pipe or heredoc)"),
        ("  send @name --file <path>", "Message from file"),
        ("  send @name --base64 <encoded>", "Message from base64 string"),
        ("", ""),
        ("", "Everything after -- is the message (no quotes needed)."),
        ("", "All flags must come before --."),
        ("", ""),
        ("Target matching:", ""),
        ("  @luna", "base name (matches luna, api-luna)"),
        ("  @api-luna", "exact full name"),
        ("  @api-", "prefix: all with tag 'api'"),
        ("  @luna:BOXE", "remote agent on another device"),
        ("", "Underscore blocks prefix: @luna does NOT match luna_reviewer_1"),
        ("", ""),
        ("Envelope:", ""),
        ("  --intent <type>", "request | inform | ack"),
        ("", "  request: expect a response"),
        ("", "  inform: FYI, no response needed"),
        ("", "  ack: replying to a request (requires --reply-to)"),
        ("  --reply-to <id>", "Link to event ID (42 or 42:BOXE)"),
        ("  --thread <name>", "Group related messages"),
        ("", ""),
        ("Sender:", ""),
        ("  --from <name>", "External sender identity (alias: -b)"),
        ("  --name <name>", "Your identity (agent name or UUID)"),
        ("", ""),
        ("Inline bundle (attach structured context):", ""),
        ("  --title <text>", "Create and attach bundle inline"),
        ("  --description <text>", "Bundle description (required with --title)"),
        ("  --events <ids>", "Event IDs/ranges: 1,2,5-10"),
        ("  --files <paths>", "Comma-separated file paths"),
        ("  --transcript <ranges>", "Format: 3-14:normal,6:full,22-30:detailed"),
        ("  --extends <id>", "Parent bundle (optional)"),
        ("", "See 'hcom bundle --help' for bundle details"),
        ("", ""),
        ("Examples:", ""),
        ("  hcom send @luna -- Hello there!", ""),
        ("  hcom send @luna @nova --intent request -- Can you help?", ""),
        ("  hcom send -- Broadcast message to everyone", ""),
        ("  echo 'Complex message' | hcom send @luna", ""),
        ("  hcom send @luna <<'EOF'", ""),
        ("  Multi-line message with special chars", ""),
        ("  EOF", ""),
    ],
    "bundle": [
        ("bundle", "List recent bundles (alias: bundle list)"),
        ("bundle list", "List recent bundles"),
        ("  --last N", "Limit count (default: 20)"),
        ("  --json", "Output JSON"),
        ("", ""),
        ("bundle cat <id>", "Expand full bundle content"),
        ("", "Shows: metadata, files (metadata only), transcript (respects detail level), events"),
        ("", ""),
        ("bundle prepare", "Show recent context, suggest template"),
        ("  --for <agent>", "Prepare for specific agent (default: self)"),
        ("  --last-transcript N", "Transcript entries to suggest (default: 20)"),
        ("  --last-events N", "Events to scan per category (default: 30)"),
        ("  --json", "Output JSON"),
        ("  --compact", "Hide how-to section"),
        ("", "Shows suggested transcript ranges, relevant events, files"),
        ("", "Outputs ready-to-use bundle create command"),
        ("", "TIP: Skip 'bundle create' — use bundle flags directly in 'hcom send'"),
        ("", ""),
        ("bundle show <id>", "Show bundle by id/prefix"),
        ("  --json", "Output JSON"),
        ("", ""),
        ('bundle create "title"', "Create bundle (positional or --title)"),
        ("  --title <text>", "Bundle title (alternative to positional)"),
        ("  --description <text>", "Bundle description (required)"),
        ("  --events 1,2,5-10", "Event IDs/ranges, comma-separated (required)"),
        ("  --files a.py,b.py", "Comma-separated file paths (required)"),
        ("  --transcript RANGES", "Transcript with detail levels (required)"),
        ("", "    Format: range:detail (3-14:normal,6:full,22-30:detailed)"),
        ("", "    normal = truncated | full = complete | detailed = tools+edits"),
        ("  --extends <id>", "Parent bundle for chaining"),
        ("  --bundle JSON", "Create from JSON payload"),
        ("  --bundle-file FILE", "Create from JSON file"),
        ("  --json", "Output JSON"),
        ("", ""),
        ("JSON format:", ""),
        ("", "{"),
        ("", '  "title": "Bundle Title",'),
        ("", '  "description": "What happened, decisions, state, next steps",'),
        ("", '  "refs": {'),
        ("", '    "events": ["123", "124-130"],'),
        ("", '    "files": ["src/auth.py", "tests/test_auth.py"],'),
        ("", '    "transcript": ["10-15:normal", "20:full", "30-35:detailed"]'),
        ("", "  },"),
        ("", '  "extends": "bundle:abc123"'),
        ("", "}"),
        ("", ""),
        ("bundle chain <id>", "Show bundle lineage"),
        ("  --json", "Output JSON"),
    ],
    "stop": [
        ("stop", "Disconnect self from hcom"),
        ("stop <name>", "Disconnect specific agent"),
        ("stop <n1> <n2> ...", "Disconnect multiple"),
        ("stop tag:<name>", "Disconnect all with tag"),
        ("stop all", "Disconnect all agents"),
        ("", ""),
    ],
    # NOTE: README references `hcom start -h` for remote/sandbox setup instructions.
    # The sandbox tip below is intentional - agents see it when running help.
    "start": [
        ("start", "Connect to hcom (from inside any AI session)"),
        ("start --as <name>", "Reclaim identity (after compaction/resume/clear)"),
        ("", ""),
        ("", ""),
        (
            "",
            "Inside a sandbox? Prefix all hcom commands with: HCOM_DIR=$PWD/.hcom",
        ),
    ],
    "kill": [
        ("kill <name>", "Kill process (+ close terminal pane)"),
        ("kill tag:<name>", "Kill all with tag"),
        ("kill all", "Kill all with tracked PIDs"),
        ("", ""),
    ],
    "listen": [
        ("listen [timeout]", "Block until message arrives"),
        ("  [timeout]", "Timeout in seconds (alias for --timeout)"),
        ("  --timeout N", "Timeout in seconds (default: 86400)"),
        ("  --json", "Output messages as JSON"),
        ("", ""),
        ("Filter flags:", ""),
        ("", "Supports all filter flags from 'events' command"),
        ("", "(--agent, --type, --status, --file, --cmd, --from, --intent, etc.)"),
        ("", "Run 'hcom events --help' for full list"),
        ("", "Filters combine with --sql using AND logic"),
        ("", ""),
        ("SQL filter mode:", ""),
        ("  --sql \"type='message'\"", "Custom SQL against events_v"),
        ("  --sql stopped:name", "Preset: wait for agent to stop"),
        ("  --idle NAME", "Shortcut: wait for agent to go idle"),
        ("", ""),
        ("Exit codes:", ""),
        ("  0", "Message received / event matched"),
        ("  1", "Timeout or error"),
        ("", ""),
        ("", "Quick unread check: hcom listen 1"),
    ],
    "reset": [
        ("reset", "Archive conversation, clear database"),
        ("reset all", "Stop all + clear db + remove hooks + reset config"),
        ("", ""),
        ("Sandbox / local mode:", ""),
        ("", "If you can't write to ~/.hcom, set:"),
        ('', '  export HCOM_DIR="$PWD/.hcom"'),
        ("", "Hooks install under $PWD (.claude/.gemini/.codex), state in $HCOM_DIR"),
        ("", ""),
        ("", "To remove local setup:"),
        ("", '  hcom hooks remove && rm -rf "$HCOM_DIR"'),
        ("", ""),
        ("", "Explicit location:"),
        ("", "  export HCOM_DIR=/your/path/.hcom"),
        ("", ""),
    ],
    "config": [
        ("config", "Show all config values"),
        ("config <key>", "Get single value"),
        ("config <key> <value>", "Set value"),
        ("config <key> --info", "Detailed help for a setting (presets, examples)"),
        ("  --json", "JSON output"),
        ("  --edit", "Open config in $EDITOR"),
        ("  --reset", "Reset config to defaults"),
        ("", ""),
        ("config terminal --setup", "Configure kitty remote control"),
        ("", ""),
        ("Per-agent runtime config:", ""),
        ("config -i <name>", "Show agent config"),
        ("config -i <name> <key>", "Get value"),
        ("config -i <name> <key> <val>", "Set value"),
        ("  -i self", "Current agent"),
        ("  keys: tag, timeout, hints, subagent_timeout", ""),
        ("", ""),
        ("Global settings:", ""),
        ("  HCOM_TAG", "Group tag (agents become tag-*)"),
        ("  HCOM_TERMINAL", 'default | <preset> | "cmd {script}"'),
        ("  HCOM_HINTS", "Text appended to all messages agent receives"),
        ("  HCOM_NOTES", "One-time notes appended to bootstrap"),
        ("  HCOM_SUBAGENT_TIMEOUT", "Subagent keep-alive seconds after task"),
        ("  HCOM_CLAUDE_ARGS", 'Default claude args (e.g. "--model opus")'),
        ("  HCOM_GEMINI_ARGS", "Default gemini args"),
        ("  HCOM_CODEX_ARGS", "Default codex args"),
        ("  HCOM_RELAY", "Relay server URL (set by 'hcom relay hf')"),
        ("  HCOM_RELAY_TOKEN", "HuggingFace token (set by 'hcom relay hf')"),
        ("  HCOM_AUTO_APPROVE", "Auto-approve safe hcom commands (1|0)"),
        ("  HCOM_AUTO_SUBSCRIBE", 'Auto-subscribe presets (e.g. "collision")'),
        ("  HCOM_NAME_EXPORT", "Export agent name to custom env var"),
        ("", ""),
        ("", "Non-HCOM_* vars in config.env pass through to claude/gemini/codex"),
        ("", "e.g. ANTHROPIC_MODEL=opus"),
        ("", ""),
        ("Precedence:", "HCOM defaults < config.env < shell env vars"),
        ("", "Each resolves independently"),
        ("", ""),
        ("", "HCOM_DIR: per project/sandbox — must be set in shell (see 'hcom reset --help')"),
    ],
    "relay": [
        ("relay", "Show relay status"),
        ("relay on", "Enable cross-device communication"),
        ("relay off", "Disable cross-device communication"),
        ("relay pull", "Force sync now"),
        ("relay hf [token]", "Setup HuggingFace Space relay"),
        ("  --update", "Update existing Space"),
        ("", ""),
        ("", "Finds or duplicates a private, free HF Space to your account."),
        ("", "Provide HF_TOKEN or run 'huggingface-cli login' first."),
        ("", "Remote agents appear with :SUFFIX (e.g. luna:BOXE)."),
    ],
    "transcript": [
        ("transcript <name>", "View agent's conversation (last 10)"),
        ("transcript <name> N", "Show exchange N"),
        ("transcript <name> N-M", "Show exchanges N through M"),
        ("transcript timeline", "User prompts across all agents by time"),
        ("  --last N", "Limit to last N exchanges (default: 10)"),
        ("  --full", "Show complete assistant responses"),
        ("  --detailed", "Show tool I/O, file edits, errors"),
        ("  --json", "JSON output"),
        ("", ""),
        ('transcript search "pattern"', "Search hcom-tracked transcripts (rg/grep)"),
        ("  --live", "Only currently alive agents"),
        ("  --all", "All transcripts (includes non-hcom sessions)"),
        ("  --limit N", "Max results (default: 20)"),
        ("  --agent TYPE", "Filter: claude | gemini | codex"),
        ("  --json", "JSON output"),
        ("", ""),
        ("", 'Tip: Reference ranges in messages instead of copying:'),
        ("", '"read my transcript range 7-10 --full"'),
    ],
    "archive": [
        ("archive", "List archived sessions (numbered)"),
        ("archive <N>", "Query events from archive (1 = most recent)"),
        ("archive <N> agents", "Query agents from archive"),
        ("archive <name>", "Query by stable name (prefix match)"),
        ("  --here", "Filter to archives from current directory"),
        ('  --sql "expr"', "SQL WHERE filter"),
        ("  --last N", "Limit events (default: 20)"),
        ("  --json", "JSON output"),
    ],
    "run": [
        ("run", "List available workflow/launch scripts and more info"),
        ("run <name> [args]", "Execute script"),
        ("run <name> --help", "Script options"),
        ("run docs", "Python API + CLI reference + examples"),
        ("", ""),
        ("", "Docs sections:"),
        ("  hcom run docs --cli", "CLI reference only"),
        ("  hcom run docs --config", "Config settings only"),
        ("  hcom run docs --api", "Python API + scripts guide"),
        ("", ""),
        ("", "User scripts: ~/.hcom/scripts/"),
    ],
    "claude": [
        ("[N] claude [args...]", "Launch N Claude agents (default N=1)"),
        ("", ""),
        _dynamic_terminal_help("claude"),
        ("  hcom N claude (N>1)", "Opens new terminal windows"),
        ('  hcom N claude "initial prompt"', "Initial prompt (positional)"),
        ('  hcom 3 claude -p "prompt"', "3 headless in background"),
        ("  HCOM_TAG=api hcom 2 claude", "Group tag (creates api-*)"),
        ("  hcom 1 claude --agent <name>", ".claude/agents/<name>.md"),
        ('  hcom 1 claude --system-prompt "text"', "System prompt"),
        ("  HCOM_TERMINAL=tmux hcom 1 claude", "detached tmux launch"),
        ("", ""),
        ("Environment:", ""),
        ("  HCOM_TAG", "Group/label tag (agents become tag-*)"),
        ("  HCOM_TERMINAL", 'default | <preset> | "cmd {script}"'),
        ("  HCOM_CLAUDE_ARGS", "Default args (merged with CLI)"),
        ("  HCOM_HINTS", "Appended to messages received"),
        ("  HCOM_NOTES", "One-time bootstrap notes"),
        ("  HCOM_SUBAGENT_TIMEOUT", "Seconds subagents are keep-alive after task"),
        ("", ""),
        ("Resume / Fork:", ""),
        ("  hcom r <name>", "Resume stopped agent by name"),
        ("  hcom f <name>", "Fork agent session (active or stopped)"),
        ("", ""),
        ("", 'Run "claude --help" for claude options.'),
        ("", 'Run "hcom config terminal --info" for terminal presets.'),
    ],
    "gemini": [
        ("[N] gemini [args...]", "Launch N Gemini agents (default N=1)"),
        ("", ""),
        _dynamic_terminal_help("gemini"),
        ("  hcom N gemini (N>1)", "Opens new terminal windows"),
        ('  hcom N gemini -i "initial prompt"', "Initial prompt (-i flag required)"),
        ("  hcom N gemini --yolo", "Flags forwarded to gemini"),
        ("  HCOM_TAG=api hcom 2 gemini", "Group tag (creates api-*)"),
        ("  HCOM_TERMINAL=tmux hcom 1 gemini", "detached tmux launch"),
        ("", ""),
        ("Environment:", ""),
        ("  HCOM_TAG", "Group tag (agents become tag-*)"),
        ("  HCOM_TERMINAL", 'default | <preset> | "cmd {script}"'),
        ("  HCOM_GEMINI_ARGS", "Default args (merged with CLI)"),
        ("  HCOM_HINTS", "Appended to messages received"),
        ("  HCOM_NOTES", "One-time bootstrap notes"),
        ("  HCOM_GEMINI_SYSTEM_PROMPT", "Use this for system prompt"),
        ("", ""),
        ("Resume:", ""),
        ("  hcom r <name>", "Resume stopped agent by name"),
        ("", "Gemini does not support session forking (hcom f)."),
        ("", ""),
        ("", 'Run "gemini --help" for Gemini CLI options.'),
        ("", 'Run "hcom config terminal --info" for terminal presets.'),
    ],
    "codex": [
        ("[N] codex [args...]", "Launch N Codex agents (default N=1)"),
        ("", ""),
        _dynamic_terminal_help("codex"),
        ("  hcom N codex (N>1)", "Opens new terminal windows"),
        ('  hcom N codex "initial prompt"', "Initial prompt (positional)"),
        ("  hcom codex --sandbox danger-full-access", "Flags forwarded to codex"),
        ("  HCOM_TAG=api hcom 2 codex", "Group tag (creates api-*)"),
        ("  HCOM_TERMINAL=tmux hcom 1 codex", "detached tmux launch"),
        ("", ""),
        ("Environment:", ""),
        ("  HCOM_TAG", "Group tag (agents become tag-*)"),
        ("  HCOM_TERMINAL", 'default | <preset> | "cmd {script}"'),
        ("  HCOM_CODEX_ARGS", "Default args (merged with CLI)"),
        ("  HCOM_HINTS", "Appended to messages received"),
        ("  HCOM_NOTES", "One-time bootstrap notes"),
        ("  HCOM_CODEX_SYSTEM_PROMPT", "System prompt (env var or config)"),
        ("", ""),
        ("Resume / Fork:", ""),
        ("  hcom r <name>", "Resume stopped agent by name"),
        ("  hcom f <name>", "Fork agent session (active or stopped)"),
        ("", ""),
        ("", 'Run "codex --help" for Codex options.'),
        ("", 'Run "hcom config terminal --info" for terminal presets.'),
    ],
    "status": [
        ("status", "Installation status and diagnostics"),
        ("status --logs", "Include recent errors and warnings"),
        ("status --json", "Machine-readable output"),
    ],
    "hooks": [
        ("hooks", "Show hook status"),
        ("hooks status", "Same as above"),
        ("hooks add [tool]", "Add hooks (claude | gemini | codex | all)"),
        ("hooks remove [tool]", "Remove hooks (claude | gemini | codex | all)"),
        ("", ""),
        ("", "Hooks enable automatic message delivery and status tracking."),
        ("", "Without hooks, use ad-hoc mode (run hcom start inside any AI tool)."),
        ("", "Restart the tool after adding hooks to activate."),
        ("", "Remove cleans both global (~/) and HCOM_DIR-local if set."),
    ],
    "term": [
        ("term", "Screen dump (all PTY instances)"),
        ("term [name]", "Screen dump for specific agent"),
        ("  --json", "Raw JSON output"),
        ("", ""),
        ("term inject <name> [text]", "Inject text into agent PTY"),
        ("  --enter", "Append \\r (submit). Works alone or with text."),
        ("", ""),
        ("term debug on", "Enable PTY debug logging (all instances)"),
        ("term debug off", "Disable PTY debug logging"),
        ("term debug logs", "List debug log files"),
        ("", ""),
        ("JSON fields:", "lines[], size[rows,cols], cursor[row,col],"),
        ("", "ready, prompt_empty, input_text"),
        ("", ""),
        ("", "Debug toggle; instances detect within ~10s."),
        ("", "Logs: ~/.hcom/.tmp/logs/pty_debug/"),
    ],
}


def get_command_help(name: str) -> str:
    """Get formatted help for a single command."""
    if name not in COMMAND_HELP:
        # Try parent command (e.g. "events sub" -> "events")
        parent = name.rsplit(" ", 1)[0]
        if parent != name and parent in COMMAND_HELP:
            return get_command_help(parent)
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
  hcom <N> claude|gemini|codex [args]   Launch agents (args forwarded to tool)
  hcom <command>                        Run command

Commands:
  send         Send message to your buddies
  listen       Block until message or event arrives
  list         Show agents, status, unread counts
  events       Query event stream, manage subscriptions
  bundle       Structured context packages for handoffs
  transcript   Read another agent's conversation
  start        Connect to hcom (run inside any AI tool)
  stop         Disconnect from hcom
  kill         Terminate agent + close terminal pane
  config       Get/set global and per-agent settings
  run          Execute workflow scripts
  relay        Cross-device communication
  archive      Query past hcom sessions
  reset        Archive and clear database
  hooks        Add or remove hooks
  status       Installation and diagnostics
  term         View/inject into agent PTY screens

Identity:
  1. Run hcom start to get a name
  2. Use --name <name> on all hcom commands

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
        "--file",
        "--base64",
        "--from",
        "-b",
        "--title",
        "--description",
        "--events",
        "--files",
        "--transcript",
        "--extends",
    },
    "events": _GLOBAL_FLAGS | _FILTER_FLAGS | {"--last", "--wait", "--sql", "--all", "--full"},
    "events sub": _GLOBAL_FLAGS | _FILTER_FLAGS | {"--once", "--for"},
    "events unsub": _GLOBAL_FLAGS,
    "events launch": _GLOBAL_FLAGS | {"--timeout"},
    "list": _GLOBAL_FLAGS | {"--json", "-v", "--verbose", "--sh", "--stopped", "--all"},
    "listen": _GLOBAL_FLAGS | _FILTER_FLAGS | {"--timeout", "--json", "--sql"},
    "start": _GLOBAL_FLAGS | {"--as"},
    "kill": _GLOBAL_FLAGS,
    "stop": _GLOBAL_FLAGS,
    "transcript": _GLOBAL_FLAGS | {"--last", "--range", "--json", "--full", "--detailed"},
    "transcript timeline": _GLOBAL_FLAGS | {"--last", "--json", "--full", "--detailed"},
    "transcript search": _GLOBAL_FLAGS | {"--limit", "--json", "--agent", "--live", "--all"},
    "config": _GLOBAL_FLAGS | {"--json", "--edit", "--reset", "-i", "--info", "--setup"},
    "reset": _GLOBAL_FLAGS,
    "relay": _GLOBAL_FLAGS | {"--space", "--update"},
    "archive": _GLOBAL_FLAGS | {"--json", "--here", "--sql", "--last"},
    "status": _GLOBAL_FLAGS | {"--json", "--logs"},
    "run": _GLOBAL_FLAGS,
    "hooks": _GLOBAL_FLAGS,
    "bundle": _GLOBAL_FLAGS | {"--json", "--last"},
    "bundle list": _GLOBAL_FLAGS | {"--json", "--last"},
    "bundle cat": _GLOBAL_FLAGS,
    "bundle show": _GLOBAL_FLAGS | {"--json"},
    "bundle prepare": _GLOBAL_FLAGS | {"--json", "--for", "--last-transcript", "--last-events", "--compact"},
    "bundle preview": _GLOBAL_FLAGS | {"--json", "--for", "--last-transcript", "--last-events", "--compact"},
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
    Stops validation at -- separator (everything after is literal content).
    """
    known = KNOWN_FLAGS.get(cmd, set())
    for arg in argv:
        if arg == "--":
            break  # Stop validation at separator
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
    """Check if running in interactive mode.

    In daemon mode, uses TTY status from context (forwarded from Rust client).
    In CLI mode, falls back to sys.stdin/stdout.isatty().

    This is necessary because daemon's main_with_context uses redirect_stdout
    which overrides the MockStdout set up by handle_cli_request.
    """
    from ..core.thread_context import get_stdin_is_tty, get_stdout_is_tty

    # Check context first (daemon mode)
    stdin_tty = get_stdin_is_tty()
    stdout_tty = get_stdout_is_tty()

    if stdin_tty is not None and stdout_tty is not None:
        # In daemon context - use forwarded TTY status
        return stdin_tty and stdout_tty

    # CLI mode - check actual TTY status
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
