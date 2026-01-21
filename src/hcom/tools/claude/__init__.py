"""Claude Code argument parsing for hcom.

This module provides CLI argument parsing and validation for Claude Code.
Hook handlers are in the top-level hooks/ package (historical location).

Exports:
    ClaudeArgsSpec: Immutable dataclass representing parsed Claude CLI arguments.
        Contains parsed tokens, flags, positionals, and semantic flags like is_background.

    resolve_claude_args: Parse arguments from CLI or environment variable.
        Returns ClaudeArgsSpec from either explicit args or HCOM_CLAUDE_ARGS env var.

    merge_claude_args: Merge environment and CLI args with precedence rules.
        CLI overrides env, with special handling for system prompts and positionals.

    add_background_defaults: Add hcom-specific defaults for headless mode.
        Adds --output-format stream-json and --verbose when -p flag is present.

    validate_conflicts: Check for conflicting flag combinations.
        Returns list of warnings (e.g., multiple system prompts).

Key Concepts:
    - Background mode: -p/--print flag enables headless execution with JSON output
    - Positional args: The prompt text passed to Claude (not flags)
    - Flag precedence: CLI args override env args, "last wins" for duplicates

Usage Example:
    >>> from hcom.tools.claude import resolve_claude_args, merge_claude_args
    >>> env_spec = resolve_claude_args(None, '--model opus')
    >>> cli_spec = resolve_claude_args(['--verbose'], None)
    >>> final_spec = merge_claude_args(env_spec, cli_spec)
"""

from .args import (
    ClaudeArgsSpec,
    resolve_claude_args,
    merge_claude_args,
    add_background_defaults,
    validate_conflicts,
)

__all__ = [
    "ClaudeArgsSpec",
    "resolve_claude_args",
    "merge_claude_args",
    "add_background_defaults",
    "validate_conflicts",
]
