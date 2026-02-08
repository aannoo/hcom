"""Hooks command - install/remove/status for tool hooks."""

from __future__ import annotations

import sys

from ..shared import CommandContext, detect_current_tool
from .utils import CLIError

# Valid tool names
HOOK_TOOLS = ("claude", "gemini", "codex")


def _get_tool_status() -> dict[str, dict]:
    """Get hook installation status for each tool."""
    from ..tools.claude.settings import (
        get_claude_settings_path,
        verify_claude_hooks_installed,
    )
    from ..tools.gemini.settings import (
        get_gemini_settings_path,
        verify_gemini_hooks_installed,
    )
    from ..tools.codex.settings import (
        get_codex_config_path,
        verify_codex_hooks_installed,
    )

    return {
        "claude": {
            "installed": verify_claude_hooks_installed(check_permissions=False),
            "path": str(get_claude_settings_path()),
        },
        "gemini": {
            "installed": verify_gemini_hooks_installed(check_permissions=False),
            "path": str(get_gemini_settings_path()),
        },
        "codex": {
            "installed": verify_codex_hooks_installed(check_permissions=False),
            "path": str(get_codex_config_path()),
        },
    }


def cmd_hooks_status(_argv: list[str]) -> int:
    """Show hook installation status for all tools."""
    status = _get_tool_status()

    for tool in HOOK_TOOLS:
        info = status[tool]
        if info["installed"]:
            print(f"{tool}:  installed    ({info['path']})")
        else:
            print(f"{tool}:  not installed")

    return 0


def cmd_hooks_add(argv: list[str]) -> int:
    """Add hooks for specified tool(s).

    Usage:
        hcom hooks add           # Auto-detect or add all
        hcom hooks add claude    # Add Claude hooks only
        hcom hooks add all       # Add all hooks
    """
    from ..core.config import get_config
    from ..tools.claude.settings import setup_claude_hooks
    from ..tools.gemini.settings import setup_gemini_hooks
    from ..tools.codex.settings import setup_codex_hooks

    # Get auto_approve setting
    try:
        config = get_config()
        include_permissions = config.auto_approve
    except Exception:
        include_permissions = False

    # Determine which tools to install
    if not argv:
        # Auto-detect current tool, or install all if not inside any
        current = detect_current_tool()
        if current in HOOK_TOOLS:
            tools = [current]
        else:
            tools = list(HOOK_TOOLS)
    elif argv[0] == "all":
        tools = list(HOOK_TOOLS)
    elif argv[0] in HOOK_TOOLS:
        tools = [argv[0]]
    else:
        raise CLIError(f"Unknown tool: {argv[0]}\nValid options: claude, gemini, codex, all")

    # Install hooks
    results = {}
    for tool in tools:
        try:
            if tool == "claude":
                setup_claude_hooks(include_permissions=include_permissions)
                results[tool] = True
            elif tool == "gemini":
                if setup_gemini_hooks(include_permissions=include_permissions):
                    results[tool] = True
                else:
                    results[tool] = False
            elif tool == "codex":
                if setup_codex_hooks(include_permissions=include_permissions):
                    results[tool] = True
                else:
                    results[tool] = False
        except Exception as e:
            print(f"error: Failed to add {tool} hooks: {e}", file=sys.stderr)
            results[tool] = False

    # Report results
    success_count = sum(1 for v in results.values() if v)
    fail_count = len(results) - success_count

    for tool, success in results.items():
        status = _get_tool_status()
        path = status[tool]["path"]
        if success:
            print(f"Added {tool} hooks  ({path})")
        else:
            print(f"Failed to add {tool} hooks", file=sys.stderr)

    if success_count > 0:
        print()
        if len(tools) == 1:
            tool_name = tools[0].title()
            if tools[0] == "claude":
                tool_name = "Claude Code"
            elif tools[0] == "gemini":
                tool_name = "Gemini CLI"
            elif tools[0] == "codex":
                tool_name = "Codex"
            print(f"Restart {tool_name} to activate hooks.")
        else:
            print("Restart the tool(s) to activate hooks.")

    return 1 if fail_count > 0 else 0


def cmd_hooks_remove(argv: list[str]) -> int:
    """Remove hooks for specified tool(s).

    Usage:
        hcom hooks remove        # Remove all hooks
        hcom hooks remove claude # Remove Claude hooks only
        hcom hooks remove all    # Remove all hooks
    """
    from ..tools.claude.settings import remove_claude_hooks
    from ..tools.gemini.settings import remove_gemini_hooks
    from ..tools.codex.settings import remove_codex_hooks

    # Determine which tools to remove
    if not argv or argv[0] == "all":
        tools = list(HOOK_TOOLS)
    elif argv[0] in HOOK_TOOLS:
        tools = [argv[0]]
    else:
        raise CLIError(f"Unknown tool: {argv[0]}\nValid options: claude, gemini, codex, all")

    # Remove hooks
    results = {}
    for tool in tools:
        try:
            if tool == "claude":
                remove_claude_hooks()
                results[tool] = True
            elif tool == "gemini":
                remove_gemini_hooks()
                results[tool] = True
            elif tool == "codex":
                remove_codex_hooks()
                results[tool] = True
        except Exception as e:
            print(f"error: Failed to remove {tool} hooks: {e}", file=sys.stderr)
            results[tool] = False

    # Report results
    for tool, success in results.items():
        if success:
            print(f"Removed {tool} hooks")
        else:
            print(f"Failed to remove {tool} hooks", file=sys.stderr)

    fail_count = sum(1 for v in results.values() if not v)
    return 1 if fail_count > 0 else 0


def cmd_hooks(argv: list[str], *, ctx: CommandContext | None = None) -> int:
    """Manage tool hooks for hcom integration.

    Usage:
        hcom hooks                  Show hook status for all tools
        hcom hooks status           Same as above
        hcom hooks add [tool]       Add hooks (claude|gemini|codex|all)
        hcom hooks remove [tool]    Remove hooks (claude|gemini|codex|all)
    """
    if not argv or argv[0] in ("--help", "-h"):
        if not argv:
            # No args = show status
            return cmd_hooks_status([])

        print("""hcom hooks - Manage tool hooks for hcom integration

Hooks enable automatic message delivery and status tracking. Without hooks,
you can still use hcom in ad-hoc mode (run hcom start in any ai tool).

Usage:
  hcom hooks                  Show hook status for all tools
  hcom hooks status           Same as above
  hcom hooks add [tool]       Add hooks (claude|gemini|codex|all)
  hcom hooks remove [tool]    Remove hooks (claude|gemini|codex|all)

Examples:
  hcom hooks add claude       Add Claude Code hooks only
  hcom hooks add              Auto-detect tool or add all
  hcom hooks remove all       Remove all hooks

After adding, restart the tool to activate hooks.""")
        return 0

    subcommand = argv[0]
    sub_argv = argv[1:]

    if subcommand == "status":
        return cmd_hooks_status(sub_argv)
    elif subcommand in ("add", "install"):
        return cmd_hooks_add(sub_argv)
    elif subcommand in ("remove", "uninstall"):
        return cmd_hooks_remove(sub_argv)
    else:
        raise CLIError(f"Unknown hooks subcommand: {subcommand}\nRun 'hcom hooks --help' for usage")


__all__ = ["cmd_hooks"]
