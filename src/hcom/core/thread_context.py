"""Thread-safe context accessors using a single contextvar.

Each accessor checks the contextvar first (daemon mode), then falls back to
os.environ (CLI mode). All context is set/reset atomically via with_context().

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
from contextvars import ContextVar
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from .hcom_context import HcomContext

# Single contextvar holding the immutable HcomContext for this thread/request.
# None when not in daemon mode (accessors fall back to os.environ).
_ctx: ContextVar[HcomContext | None] = ContextVar("hcom_ctx", default=None)


# === Thread-Safe Accessors ===
# Each checks _ctx first (daemon mode), falls back to os.environ (CLI mode)


def get_process_id() -> str | None:
    """Get HCOM_PROCESS_ID - identifies launched instances."""
    ctx = _ctx.get()
    if ctx is not None:
        return ctx.process_id
    return os.environ.get("HCOM_PROCESS_ID") or None


def get_is_launched() -> bool:
    """Get HCOM_LAUNCHED - True if launched by hcom."""
    ctx = _ctx.get()
    if ctx is not None:
        return ctx.is_launched
    return os.environ.get("HCOM_LAUNCHED") == "1"


def get_is_pty_mode() -> bool:
    """Get HCOM_PTY_MODE - True if running in PTY wrapper."""
    ctx = _ctx.get()
    if ctx is not None:
        return ctx.is_pty_mode
    return os.environ.get("HCOM_PTY_MODE") == "1"


def get_background_name() -> str | None:
    """Get HCOM_BACKGROUND - log filename for background mode."""
    ctx = _ctx.get()
    if ctx is not None:
        return ctx.background_name
    return os.environ.get("HCOM_BACKGROUND") or None


def get_hcom_dir() -> Path | None:
    """Get HCOM_DIR - custom hcom data directory.

    Returns:
        Path to hcom directory if HCOM_DIR set, None for default (~/.hcom).
    """
    ctx = _ctx.get()
    if ctx is not None:
        return ctx.hcom_dir
    hcom_dir = os.environ.get("HCOM_DIR")
    if hcom_dir:
        return Path(hcom_dir).expanduser()
    return None


def get_hcom_dir_str() -> str | None:
    """Get HCOM_DIR as string, only if explicitly set (not defaulted).

    Used by is_hcom_dir_override() to check if user provided custom path.
    """
    ctx = _ctx.get()
    if ctx is not None:
        if ctx.hcom_dir_override:
            return str(ctx.hcom_dir) if ctx.hcom_dir else None
        return None
    return os.environ.get("HCOM_DIR") or None


def get_cwd() -> Path:
    """Get current working directory - thread-safe.

    In daemon mode, returns the cwd captured at request start.
    In CLI mode, returns Path.cwd().
    """
    ctx = _ctx.get()
    if ctx is not None:
        return ctx.cwd
    return Path.cwd()


def get_launched_by() -> str | None:
    """Get HCOM_LAUNCHED_BY - name of instance that launched this one."""
    ctx = _ctx.get()
    if ctx is not None:
        return ctx.launched_by
    return os.environ.get("HCOM_LAUNCHED_BY") or None


def get_launch_batch_id() -> str | None:
    """Get HCOM_LAUNCH_BATCH_ID - batch identifier for grouped launches."""
    ctx = _ctx.get()
    if ctx is not None:
        return ctx.launch_batch_id
    return os.environ.get("HCOM_LAUNCH_BATCH_ID") or None


def get_launch_event_id() -> str | None:
    """Get HCOM_LAUNCH_EVENT_ID - event ID for this launch."""
    ctx = _ctx.get()
    if ctx is not None:
        return ctx.launch_event_id
    return os.environ.get("HCOM_LAUNCH_EVENT_ID") or None


def get_launched_preset() -> str | None:
    """Get HCOM_LAUNCHED_PRESET - terminal preset used to launch this instance."""
    ctx = _ctx.get()
    if ctx is not None:
        return ctx.launched_preset
    return os.environ.get("HCOM_LAUNCHED_PRESET") or None


def get_stdin_is_tty() -> bool | None:
    """Get stdin TTY status from context.

    Returns True/False in daemon mode, None in CLI mode (caller uses sys.stdin.isatty()).
    """
    ctx = _ctx.get()
    if ctx is not None:
        return ctx.stdin_is_tty
    return None


def get_stdout_is_tty() -> bool | None:
    """Get stdout TTY status from context.

    Returns True/False in daemon mode, None in CLI mode (caller uses sys.stdout.isatty()).
    """
    ctx = _ctx.get()
    if ctx is not None:
        return ctx.stdout_is_tty
    return None


def get_is_claude() -> bool:
    """Get Claude tool marker - True if running inside Claude Code.

    Checks CLAUDECODE=1 or CLAUDE_ENV_FILE presence.
    """
    ctx = _ctx.get()
    if ctx is not None:
        return ctx.is_claude
    return os.environ.get("CLAUDECODE") == "1" or bool(os.environ.get("CLAUDE_ENV_FILE"))


def get_is_gemini() -> bool:
    """Get Gemini tool marker - True if running inside Gemini CLI."""
    ctx = _ctx.get()
    if ctx is not None:
        return ctx.is_gemini
    return os.environ.get("GEMINI_CLI") == "1"


def get_is_codex() -> bool:
    """Get Codex tool marker - True if running inside Codex.

    Checks any CODEX_* env var presence.
    """
    ctx = _ctx.get()
    if ctx is not None:
        return ctx.is_codex
    return (
        "CODEX_SANDBOX" in os.environ
        or "CODEX_SANDBOX_NETWORK_DISABLED" in os.environ
        or "CODEX_MANAGED_BY_NPM" in os.environ
        or "CODEX_MANAGED_BY_BUN" in os.environ
        or "CODEX_THREAD_ID" in os.environ
    )


def get_hcom_go() -> bool:
    """Get HCOM_GO - True if gating prompts should be bypassed."""
    ctx = _ctx.get()
    if ctx is not None:
        return ctx.hcom_go
    return os.environ.get("HCOM_GO") == "1"


def get_codex_thread_id() -> str | None:
    """Get Codex thread ID (session equivalent)."""
    ctx = _ctx.get()
    if ctx is not None:
        return ctx.codex_thread_id
    return os.environ.get("CODEX_THREAD_ID") or None


def get_hcom_notes_text() -> str:
    """Get HCOM_NOTES - per-instance bootstrap user notes."""
    ctx = _ctx.get()
    if ctx is not None:
        return ctx.notes
    return os.environ.get("HCOM_NOTES") or ""


def is_in_daemon_mode() -> bool:
    """Check if running inside daemon's with_context().

    Used to prevent os.execve() calls that would kill the daemon process.
    """
    return _ctx.get() is not None


# === Context Manager ===


@contextmanager
def with_context(ctx: "HcomContext"):
    """Set context for the duration of the block.

    Thread-safe: contextvars are per-coroutine/per-thread.
    Concurrent daemon requests each see their own context values.

    Args:
        ctx: Immutable execution context to apply.
    """
    token = _ctx.set(ctx)
    try:
        yield
    finally:
        _ctx.reset(token)


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
