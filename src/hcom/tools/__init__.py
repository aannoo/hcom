"""Tool-specific integrations for hcom multi-agent communication.

This package contains CLI-specific integrations for each supported AI coding tool.
Each tool has its own subpackage handling:
- Argument parsing and validation (args.py)
- Hook handlers for lifecycle events (hooks.py)
- Settings/config file management (settings.py)

Structure:
    tools/
    ├── claude/     # Claude Code argument parsing (hooks in hooks/ package)
    ├── gemini/     # Gemini CLI hooks, settings, and idle detection
    └── codex/      # Codex CLI hooks, settings, and transcript parsing

Architecture Notes:
    - Claude Code hooks are in the root hooks/ package (historical, may consolidate)
    - All tools share common patterns: args parsing, hook handlers, settings management
    - Each tool has specific quirks handled in its respective subpackage
"""

__all__ = ["gemini", "codex"]
