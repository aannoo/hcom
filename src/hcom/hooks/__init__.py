"""Claude Code hook system for HCOM multi-agent communication.

This module integrates HCOM with Claude Code's hook system, enabling:
- Session lifecycle tracking (SessionStart, SessionEnd)
- Message delivery via UserPromptSubmit and PostToolUse hooks
- Status tracking via PreToolUse, Stop, and Notification hooks
- Subagent coordination via SubagentStart/SubagentStop hooks

Architecture:
    dispatcher.py  - Single entry point, routes hooks to parent/subagent handlers
    parent.py      - Hook handlers for parent (top-level) Claude instances
    subagent.py    - Hook handlers for Task tool subagents
    family.py      - Shared helpers (message polling, TCP notification)
    settings.py    - Claude settings.json management (hook installation)
    utils.py       - Context initialization and utility re-exports

Hook Flow:
    Claude Code invokes: hcom <hook_type>
    → dispatcher.handle_hook() routes based on context
    → parent.* or subagent.* handlers execute
    → Hook output printed as JSON to stdout

Entry Points:
    handle_hook()           - CLI hook dispatcher (called by claude hooks)
    setup_claude_hooks()    - Install hooks into ~/.claude/settings.json
    verify_claude_hooks_installed() - Check hook installation status
    remove_claude_hooks()   - Uninstall hooks from settings.json
"""

from .dispatcher import handle_hook
from .settings import (
    CLAUDE_HOOK_CONFIGS,
    CLAUDE_HOOK_TYPES,
    CLAUDE_HOOK_COMMANDS,
    CLAUDE_HCOM_HOOK_PATTERNS,
    get_claude_settings_path,
    load_claude_settings,
    _remove_claude_hcom_hooks,
    setup_claude_hooks,
    verify_claude_hooks_installed,
    remove_claude_hooks,
)

__all__ = [
    "handle_hook",
    "CLAUDE_HOOK_CONFIGS",
    "CLAUDE_HOOK_TYPES",
    "CLAUDE_HOOK_COMMANDS",
    "CLAUDE_HCOM_HOOK_PATTERNS",
    "get_claude_settings_path",
    "load_claude_settings",
    "_remove_claude_hcom_hooks",
    "setup_claude_hooks",
    "verify_claude_hooks_installed",
    "remove_claude_hooks",
]
