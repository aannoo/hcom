"""Thread-safe context accessors using contextvars.

Replaces direct os.environ.get() calls with thread-safe accessors that use
Python's contextvars module. This enables concurrent daemon requests without
race conditions on global state.

Design:
- Daemon mode: contextvars set at request start via with_context()
- CLI mode: contextvars empty, accessors fall back to os.environ
- All existing code continues to work unchanged

Usage:
    # Daemon entry point:
    with with_context(ctx):
        result = main(argv)  # All code sees correct context

    # Anywhere in codebase:
    process_id = get_process_id()  # Thread-safe, uses contextvar or os.environ
"""

from __future__ import annotations

import os
from contextlib import contextmanager
from contextvars import ContextVar, Token
from pathlib import Path
from typing import Any, TYPE_CHECKING

if TYPE_CHECKING:
    from .hcom_context import HcomContext

# === Context Variables ===
# Each stores the value from HcomContext when set, or None when not in daemon mode

_ctx_process_id: ContextVar[str | None] = ContextVar("hcom_process_id", default=None)
_ctx_is_launched: ContextVar[bool | None] = ContextVar("hcom_is_launched", default=None)
_ctx_is_pty_mode: ContextVar[bool | None] = ContextVar("hcom_is_pty_mode", default=None)
_ctx_background_name: ContextVar[str | None] = ContextVar("hcom_background_name", default=None)
_ctx_hcom_dir: ContextVar[Path | None] = ContextVar("hcom_hcom_dir", default=None)
_ctx_hcom_dir_override: ContextVar[bool | None] = ContextVar("hcom_hcom_dir_override", default=None)
_ctx_cwd: ContextVar[Path | None] = ContextVar("hcom_cwd", default=None)
_ctx_launched_by: ContextVar[str | None] = ContextVar("hcom_launched_by", default=None)
_ctx_launch_batch_id: ContextVar[str | None] = ContextVar("hcom_launch_batch_id", default=None)
_ctx_launch_event_id: ContextVar[str | None] = ContextVar("hcom_launch_event_id", default=None)
_ctx_launched_preset: ContextVar[str | None] = ContextVar("hcom_launched_preset", default=None)
# TTY status from client - needed for is_interactive() in daemon mode
# None = not in context (use sys.stdin/stdout.isatty()), bool = explicit value from context
_ctx_stdin_is_tty: ContextVar[bool | None] = ContextVar("hcom_stdin_is_tty", default=None)
_ctx_stdout_is_tty: ContextVar[bool | None] = ContextVar("hcom_stdout_is_tty", default=None)
# Tool markers for context-based detection (daemon-safe)
# None = not in context (use os.environ fallback), bool = explicit value from context
_ctx_is_claude: ContextVar[bool | None] = ContextVar("hcom_is_claude", default=None)
_ctx_is_gemini: ContextVar[bool | None] = ContextVar("hcom_is_gemini", default=None)
_ctx_is_codex: ContextVar[bool | None] = ContextVar("hcom_is_codex", default=None)
_ctx_hcom_go: ContextVar[bool | None] = ContextVar("hcom_hcom_go", default=None)
# Codex thread ID (session equivalent) - None = not in context (use os.environ fallback)
_ctx_codex_thread_id: ContextVar[str | None] = ContextVar("hcom_codex_thread_id", default=None)
# Per-instance HCOM_NOTES (bootstrap user notes)
_ctx_notes: ContextVar[str | None] = ContextVar("hcom_notes", default=None)
# Daemon mode marker - True when running inside daemon's with_context()
_ctx_in_daemon: ContextVar[bool] = ContextVar("hcom_in_daemon", default=False)


# === Thread-Safe Accessors ===
# Each checks contextvar first (daemon mode), falls back to os.environ (CLI mode)


def get_process_id() -> str | None:
    """Get HCOM_PROCESS_ID - identifies launched instances.

    Returns:
        Process ID string if set, None otherwise.
    """
    val = _ctx_process_id.get()
    if val is not None:
        return val
    return os.environ.get("HCOM_PROCESS_ID") or None


def get_is_launched() -> bool:
    """Get HCOM_LAUNCHED - True if launched by hcom.

    Returns:
        True if HCOM_LAUNCHED=1, False otherwise.
    """
    val = _ctx_is_launched.get()
    if val is not None:
        return val
    return os.environ.get("HCOM_LAUNCHED") == "1"


def get_is_pty_mode() -> bool:
    """Get HCOM_PTY_MODE - True if running in PTY wrapper.

    Returns:
        True if HCOM_PTY_MODE=1, False otherwise.
    """
    val = _ctx_is_pty_mode.get()
    if val is not None:
        return val
    return os.environ.get("HCOM_PTY_MODE") == "1"


def get_background_name() -> str | None:
    """Get HCOM_BACKGROUND - log filename for background mode.

    Returns:
        Background name string if set, None otherwise.
    """
    val = _ctx_background_name.get()
    if val is not None:
        return val
    return os.environ.get("HCOM_BACKGROUND") or None


def get_hcom_dir() -> Path | None:
    """Get HCOM_DIR - custom hcom data directory.

    Returns:
        Path to hcom directory if HCOM_DIR set, None for default (~/.hcom).
    """
    val = _ctx_hcom_dir.get()
    if val is not None:
        return val
    hcom_dir = os.environ.get("HCOM_DIR")
    if hcom_dir:
        return Path(hcom_dir).expanduser()
    return None


def get_hcom_dir_str() -> str | None:
    """Get HCOM_DIR as string - for env var override checks.

    Returns the HCOM_DIR value ONLY if it was explicitly set (not defaulted).
    Used by is_hcom_dir_override() to check if user provided custom path.

    Returns:
        HCOM_DIR value if explicitly set, None if using default (~/.hcom).
    """
    # In daemon mode, check the override flag
    override = _ctx_hcom_dir_override.get()
    if override is not None:
        # Context is set - return dir string only if it was an explicit override
        if override:
            val = _ctx_hcom_dir.get()
            return str(val) if val else None
        return None
    # CLI mode - fall back to os.environ
    return os.environ.get("HCOM_DIR") or None


def get_cwd() -> Path:
    """Get current working directory - thread-safe.

    In daemon mode, returns the cwd from context (captured at request start).
    In CLI mode, returns Path.cwd().

    Returns:
        Current working directory as Path.
    """
    val = _ctx_cwd.get()
    if val is not None:
        return val
    return Path.cwd()


def get_launched_by() -> str | None:
    """Get HCOM_LAUNCHED_BY - name of instance that launched this one.

    Returns:
        Launcher name if set, None otherwise.
    """
    val = _ctx_launched_by.get()
    if val is not None:
        return val
    return os.environ.get("HCOM_LAUNCHED_BY") or None


def get_launch_batch_id() -> str | None:
    """Get HCOM_LAUNCH_BATCH_ID - batch identifier for grouped launches.

    Returns:
        Batch ID if set, None otherwise.
    """
    val = _ctx_launch_batch_id.get()
    if val is not None:
        return val
    return os.environ.get("HCOM_LAUNCH_BATCH_ID") or None


def get_launch_event_id() -> str | None:
    """Get HCOM_LAUNCH_EVENT_ID - event ID for this launch.

    Returns:
        Event ID if set, None otherwise.
    """
    val = _ctx_launch_event_id.get()
    if val is not None:
        return val
    return os.environ.get("HCOM_LAUNCH_EVENT_ID") or None


def get_launched_preset() -> str | None:
    """Get HCOM_LAUNCHED_PRESET - terminal preset used to launch this instance."""
    val = _ctx_launched_preset.get()
    if val is not None:
        return val
    return os.environ.get("HCOM_LAUNCHED_PRESET") or None


def get_stdin_is_tty() -> bool | None:
    """Get stdin TTY status from context.

    In daemon mode, returns the client's stdin TTY status.
    In CLI mode, returns None (caller should use sys.stdin.isatty()).

    Returns:
        True/False if in daemon context, None if not in context.
    """
    return _ctx_stdin_is_tty.get()


def get_stdout_is_tty() -> bool | None:
    """Get stdout TTY status from context.

    In daemon mode, returns the client's stdout TTY status.
    In CLI mode, returns None (caller should use sys.stdout.isatty()).

    Returns:
        True/False if in daemon context, None if not in context.
    """
    return _ctx_stdout_is_tty.get()


def get_is_claude() -> bool:
    """Get Claude tool marker - True if running inside Claude Code.

    Checks CLAUDECODE=1 or CLAUDE_ENV_FILE presence.
    In daemon mode, uses context. In CLI mode, falls back to os.environ.

    Returns:
        True if inside Claude Code, False otherwise.
    """
    val = _ctx_is_claude.get()
    if val is not None:
        return val
    return os.environ.get("CLAUDECODE") == "1" or bool(os.environ.get("CLAUDE_ENV_FILE"))


def get_is_gemini() -> bool:
    """Get Gemini tool marker - True if running inside Gemini CLI.

    Checks GEMINI_CLI=1.
    In daemon mode, uses context. In CLI mode, falls back to os.environ.

    Returns:
        True if inside Gemini CLI, False otherwise.
    """
    val = _ctx_is_gemini.get()
    if val is not None:
        return val
    return os.environ.get("GEMINI_CLI") == "1"


def get_is_codex() -> bool:
    """Get Codex tool marker - True if running inside Codex.

    Checks any CODEX_* env var presence.
    In daemon mode, uses context. In CLI mode, falls back to os.environ.

    Returns:
        True if inside Codex, False otherwise.
    """
    val = _ctx_is_codex.get()
    if val is not None:
        return val
    return (
        "CODEX_SANDBOX" in os.environ
        or "CODEX_SANDBOX_NETWORK_DISABLED" in os.environ
        or "CODEX_MANAGED_BY_NPM" in os.environ
        or "CODEX_MANAGED_BY_BUN" in os.environ
        or "CODEX_THREAD_ID" in os.environ
    )


def get_hcom_go() -> bool:
    """Get HCOM_GO - True if gating prompts should be bypassed.

    In daemon mode, uses context. In CLI mode, falls back to os.environ.

    Returns:
        True if HCOM_GO=1, False otherwise.
    """
    val = _ctx_hcom_go.get()
    if val is not None:
        return val
    return os.environ.get("HCOM_GO") == "1"


def get_codex_thread_id() -> str | None:
    """Get Codex thread ID (session equivalent).

    In daemon mode, uses context. In CLI mode, falls back to os.environ.

    Returns:
        Thread ID string or None if not in Codex.
    """
    val = _ctx_codex_thread_id.get()
    if val is not None:
        return val
    return os.environ.get("CODEX_THREAD_ID") or None


def get_hcom_notes_text() -> str:
    """Get HCOM_NOTES - per-instance bootstrap user notes.

    Returns:
        Notes string if set, empty string otherwise.
    """
    val = _ctx_notes.get()
    if val is not None:
        return val
    return os.environ.get("HCOM_NOTES") or ""


def is_in_daemon_mode() -> bool:
    """Check if running inside daemon's with_context().

    Used to prevent os.execve() calls that would kill the daemon process.
    In daemon mode, commands that would normally run in current terminal
    should instead open a new terminal window.

    Returns:
        True if in daemon context, False otherwise.
    """
    return _ctx_in_daemon.get()


# === Context Manager ===


@contextmanager
def with_context(ctx: "HcomContext"):
    """Set all context variables from HcomContext for the duration of the block.

    Thread-safe: uses contextvars which are per-coroutine/per-thread.
    Concurrent daemon requests each see their own context values.

    Args:
        ctx: Immutable execution context to apply.

    Yields:
        None - all code in the block sees the context values via accessors.

    Example:
        ctx = HcomContext.from_env(request.env, request.cwd)
        with with_context(ctx):
            # get_process_id() returns ctx.process_id
            # get_cwd() returns ctx.cwd
            result = main(argv)
    """
    # Save tokens for reset
    tokens: list[Token[Any]] = []

    # Set all context variables
    tokens.append(_ctx_process_id.set(ctx.process_id))
    tokens.append(_ctx_is_launched.set(ctx.is_launched))
    tokens.append(_ctx_is_pty_mode.set(ctx.is_pty_mode))
    tokens.append(_ctx_background_name.set(ctx.background_name))
    tokens.append(_ctx_hcom_dir.set(ctx.hcom_dir))
    tokens.append(_ctx_hcom_dir_override.set(ctx.hcom_dir_override))
    tokens.append(_ctx_cwd.set(ctx.cwd))

    # Launch context fields
    tokens.append(_ctx_launched_by.set(ctx.launched_by))
    tokens.append(_ctx_launch_batch_id.set(ctx.launch_batch_id))
    tokens.append(_ctx_launch_event_id.set(ctx.launch_event_id))
    tokens.append(_ctx_launched_preset.set(ctx.launched_preset))

    # TTY status - always present in HcomContext
    tokens.append(_ctx_stdin_is_tty.set(ctx.stdin_is_tty))
    tokens.append(_ctx_stdout_is_tty.set(ctx.stdout_is_tty))

    # Tool markers - always present in HcomContext
    tokens.append(_ctx_is_claude.set(ctx.is_claude))
    tokens.append(_ctx_is_gemini.set(ctx.is_gemini))
    tokens.append(_ctx_is_codex.set(ctx.is_codex))
    tokens.append(_ctx_hcom_go.set(ctx.hcom_go))
    tokens.append(_ctx_codex_thread_id.set(ctx.codex_thread_id))
    tokens.append(_ctx_notes.set(ctx.notes))

    # Mark that we're in daemon mode
    tokens.append(_ctx_in_daemon.set(True))

    try:
        yield
    finally:
        # Reset all context variables to previous values
        for token in tokens:
            token.var.reset(token)


__all__ = [
    # Accessors
    "get_process_id",
    "get_is_launched",
    "get_is_pty_mode",
    "get_background_name",
    "get_hcom_dir",
    "get_hcom_dir_str",
    "get_cwd",
    "get_launched_by",
    "get_launch_batch_id",
    "get_launch_event_id",
    "get_launched_preset",
    "get_stdin_is_tty",
    "get_stdout_is_tty",
    # Tool detection
    "get_is_claude",
    "get_is_gemini",
    "get_is_codex",
    "get_hcom_go",
    "get_codex_thread_id",
    "get_hcom_notes_text",
    # Daemon mode
    "is_in_daemon_mode",
    # Context manager
    "with_context",
]
