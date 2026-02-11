"""Shim command - install/uninstall PATH shims for claude/gemini/codex."""

from __future__ import annotations

import os
import shutil
from pathlib import Path

from ..shared import CommandContext
from .utils import CLIError

# Tools that can be shimmed
SHIM_TOOLS = ("claude", "gemini", "codex")

# Default shim directory
DEFAULT_SHIM_DIR = Path.home() / ".local" / "ai-shims"

# Shell RC files to check/modify
SHELL_RC_FILES = {
    "zsh": Path.home() / ".zshrc",
    "bash": Path.home() / ".bashrc",
}

# The PATH export line we add
PATH_EXPORT_LINE = 'export PATH="$HOME/.local/ai-shims:$PATH"'
PATH_COMMENT = "# hcom shim"


def _get_hcom_path() -> str | None:
    """Get path to hcom executable."""
    return shutil.which("hcom")


def _get_current_shell() -> str:
    """Detect current shell (zsh or bash)."""
    shell = os.environ.get("SHELL", "")
    if "zsh" in shell:
        return "zsh"
    return "bash"  # Default to bash


def _get_rc_file() -> Path:
    """Get the appropriate shell rc file."""
    shell = _get_current_shell()
    return SHELL_RC_FILES.get(shell, SHELL_RC_FILES["bash"])


def _rc_has_shim_path(rc_file: Path) -> bool:
    """Check if rc file already has the shim PATH line."""
    if not rc_file.exists():
        return False
    content = rc_file.read_text()
    return PATH_EXPORT_LINE in content


def _add_shim_path_to_rc(rc_file: Path) -> bool:
    """Add shim PATH line to rc file. Returns True if added."""
    if _rc_has_shim_path(rc_file):
        return False  # Already present

    with open(rc_file, "a") as f:
        f.write(f"\n{PATH_COMMENT}\n{PATH_EXPORT_LINE}\n")
    return True


def _remove_shim_path_from_rc(rc_file: Path) -> bool:
    """Remove shim PATH line from rc file. Returns True if removed."""
    if not rc_file.exists():
        return False

    content = rc_file.read_text()
    if PATH_EXPORT_LINE not in content:
        return False

    # Remove the comment and export line
    lines = content.splitlines()
    new_lines = []
    skip_next = False
    for line in lines:
        if line.strip() == PATH_COMMENT:
            skip_next = True
            continue
        if skip_next and PATH_EXPORT_LINE in line:
            skip_next = False
            continue
        if PATH_EXPORT_LINE in line:
            continue  # Remove even without comment
        new_lines.append(line)

    # Write back, removing trailing empty lines from our removal
    new_content = "\n".join(new_lines)
    # Clean up multiple trailing newlines
    while new_content.endswith("\n\n"):
        new_content = new_content[:-1]
    if not new_content.endswith("\n"):
        new_content += "\n"

    rc_file.write_text(new_content)
    return True


def _find_real_binary(tool: str, shim_dir: Path) -> str | None:
    """Find real binary excluding shim dir from PATH (handles symlinks/trailing slashes)."""
    norm_shim = os.path.realpath(str(shim_dir)).rstrip("/")
    path_parts = os.environ.get("PATH", "").split(":")
    clean_path = ":".join(p for p in path_parts if os.path.realpath(p).rstrip("/") != norm_shim)
    old_path = os.environ.get("PATH")
    try:
        os.environ["PATH"] = clean_path
        return shutil.which(tool)
    finally:
        if old_path is None:
            del os.environ["PATH"]
        else:
            os.environ["PATH"] = old_path


def _get_shim_status() -> dict[str, dict]:
    """Get status of each tool's shim."""
    shim_dir = DEFAULT_SHIM_DIR
    result = {}
    for tool in SHIM_TOOLS:
        shim_path = shim_dir / tool
        shim_exists = shim_path.exists() and shim_path.is_symlink()
        real_path = _find_real_binary(tool, shim_dir) if shim_exists else shutil.which(tool)
        which_result = shutil.which(tool)
        active = shim_exists and which_result == str(shim_path)
        result[tool] = {
            "shimmed": shim_exists,
            "shim_path": str(shim_path) if shim_exists else None,
            "real_path": real_path,
            "active": active,
        }
    return result


def cmd_shim_install(argv: list[str]) -> int:
    """Install shims for claude/gemini/codex."""
    hcom_path = _get_hcom_path()
    if not hcom_path:
        raise CLIError("Cannot find hcom executable in PATH")

    shim_dir = DEFAULT_SHIM_DIR
    rc_file = _get_rc_file()

    # Create shim directory
    shim_dir.mkdir(parents=True, exist_ok=True)

    # Create symlinks
    created = []
    for tool in SHIM_TOOLS:
        shim_path = shim_dir / tool
        if shim_path.exists() or shim_path.is_symlink():
            if shim_path.is_symlink():
                shim_path.unlink()
            elif shim_path.is_dir():
                raise CLIError(f"Cannot install: {shim_path} is a directory")
            else:
                raise CLIError(f"Cannot install: {shim_path} exists and is not a symlink")
        shim_path.symlink_to(hcom_path)
        created.append(tool)

    # Add PATH to rc file
    rc_modified = _add_shim_path_to_rc(rc_file)

    # Output
    print(f"Created symlinks in {shim_dir}/")
    for tool in created:
        print(f"  {tool} → {hcom_path}")

    if rc_modified:
        print(f"\nAdded to {rc_file}:")
        print(f"  {PATH_EXPORT_LINE}")
        print("\nRestart your shell or run:")
        print(f"  source {rc_file}")
    else:
        print(f"\nPATH already configured in {rc_file}")

    # Check if PATH is currently correct
    current_path = os.environ.get("PATH", "")
    if str(shim_dir) not in current_path:
        print("\nNote: Shim directory not in current PATH.")
        print(f"Run 'source {rc_file}' or restart your shell.")

    return 0


def cmd_shim_uninstall(argv: list[str]) -> int:
    """Remove shims for claude/gemini/codex."""
    shim_dir = DEFAULT_SHIM_DIR
    rc_file = _get_rc_file()

    # Remove symlinks (only unlink actual symlinks)
    removed = []
    skipped = []
    for tool in SHIM_TOOLS:
        shim_path = shim_dir / tool
        if shim_path.is_symlink():
            shim_path.unlink()
            removed.append(tool)
        elif shim_path.exists():
            skipped.append((tool, "not a symlink"))

    # Remove PATH from rc file
    rc_modified = _remove_shim_path_from_rc(rc_file)

    # Try to remove shim directory if empty
    dir_removed = False
    if shim_dir.exists():
        try:
            shim_dir.rmdir()  # Only succeeds if empty
            dir_removed = True
        except OSError:
            pass

    # Output
    if removed:
        print(f"Removed symlinks from {shim_dir}/")
        for tool in removed:
            print(f"  {tool}")
    else:
        print("No shims found to remove.")

    if skipped:
        print("\nSkipped (not symlinks):")
        for tool, reason in skipped:
            print(f"  {tool}: {reason}")

    if dir_removed:
        print(f"\nRemoved empty directory: {shim_dir}")

    if rc_modified:
        print(f"\nRemoved PATH line from {rc_file}")
        print("\nRestart your shell or run:")
        print(f"  source {rc_file}")

    return 0


def cmd_shim_status(argv: list[str]) -> int:
    """Show status of shims."""
    status = _get_shim_status()
    rc_file = _get_rc_file()
    rc_configured = _rc_has_shim_path(rc_file)
    shim_dir = DEFAULT_SHIM_DIR

    # Check if shim dir is in current PATH
    current_path = os.environ.get("PATH", "")
    path_active = str(shim_dir) in current_path

    print("Shim Status:")
    print(f"  Directory: {shim_dir}")
    print(f"  RC file: {rc_file} ({'configured' if rc_configured else 'not configured'})")
    print(f"  PATH: {'active' if path_active else 'not in current PATH'}")
    print()

    print("Tools:")
    warnings = []
    for tool, info in status.items():
        if info["shimmed"]:
            if info["active"]:
                status_str = "✓ shimmed (active)"
                if not info["real_path"]:
                    warnings.append(f"{tool}: shim active but real binary not found")
            else:
                status_str = "⚠ shimmed (not in PATH)"
        else:
            status_str = "✗ not shimmed"

        real = info["real_path"] or "not installed"
        print(f"  {tool}: {status_str}")
        print(f"    real binary: {real}")

    if warnings:
        print("\nWarnings:")
        for w in warnings:
            print(f"  ⚠ {w}")

    return 0


def cmd_shim(argv: list[str], *, ctx: CommandContext | None = None) -> int:
    """Manage PATH shims for claude/gemini/codex.

    Usage:
        hcom shim install    Install shims (symlinks + rc file)
        hcom shim uninstall  Remove shims
        hcom shim status     Show shim status
    """
    if not argv or argv[0] in ("--help", "-h"):
        print("""hcom shim - Manage PATH shims for claude/gemini/codex

When installed, running 'claude', 'gemini', or 'codex' will automatically
use hcom's wrapper, enabling multi-agent communication for any tool that
calls these commands (including Claude Squad, CCManager, etc).

Usage:
  hcom shim install      Create symlinks and configure shell PATH
  hcom shim uninstall    Remove symlinks and PATH configuration
  hcom shim status       Show current shim status

The shim works by:
  1. Creating symlinks in ~/.local/ai-shims/ pointing to hcom
  2. Adding that directory to your PATH (in .bashrc/.zshrc)
  3. hcom detects it was invoked as 'claude' and wraps accordingly""")
        return 0

    subcommand = argv[0]
    sub_argv = argv[1:]

    if subcommand == "install":
        return cmd_shim_install(sub_argv)
    elif subcommand in ("uninstall", "remove"):
        return cmd_shim_uninstall(sub_argv)
    elif subcommand == "status":
        return cmd_shim_status(sub_argv)
    else:
        raise CLIError(f"Unknown shim subcommand: {subcommand}\nValid: install, uninstall, status")


__all__ = ["cmd_shim"]
