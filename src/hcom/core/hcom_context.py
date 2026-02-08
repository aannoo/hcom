"""Immutable context for all hcom operations.

HcomContext captures the execution environment at a point in time, enabling:
- Daemon integration: context passed explicitly rather than read from os.environ
- Testing: inject controlled context without modifying globals
- Debugging: context snapshot shows exact state when operation ran

Design:
- Frozen dataclass: prevents accidental mutation, thread-safe
- from_env(): builds from explicit env dict (for daemon/testing)
- from_os(): convenience wrapper using os.environ + Path.cwd()
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path
from typing import Literal


ToolType = Literal["claude", "gemini", "codex", "adhoc"]


@dataclass(frozen=True)
class HcomContext:
    """Immutable context for all hcom operations.

    Captures execution environment for hook handlers and CLI commands.
    Enables daemon integration where global state isn't available.

    Attributes:
        process_id: HCOM_PROCESS_ID for launched instances (None for vanilla/adhoc).
        is_launched: True if HCOM_LAUNCHED=1 (hcom-launched instance).
        is_pty_mode: True if HCOM_PTY_MODE=1 (running in PTY wrapper).
        is_background: True if HCOM_BACKGROUND is set (background/headless mode).
        background_name: Log filename for background mode (from HCOM_BACKGROUND).
        hcom_dir: Path to HCOM data directory (~/.hcom or HCOM_DIR).
        hcom_dir_override: True if HCOM_DIR was explicitly set (not defaulted).
        cwd: Current working directory when context was captured.
        tool: Tool type (claude/gemini/codex/adhoc).
        claude_env_file: CLAUDE_ENV_FILE path (for session ID extraction).
        stdin_is_tty: True if client's stdin is a TTY (for is_interactive checks).
        stdout_is_tty: True if client's stdout is a TTY (for is_interactive checks).
        is_claude: True if CLAUDECODE=1 or CLAUDE_ENV_FILE set (for tool detection).
        is_gemini: True if GEMINI_CLI=1 (for tool detection).
        is_codex: True if any CODEX_* env var present (for tool detection).
        hcom_go: True if HCOM_GO=1 (bypass gating prompts).
    """

    process_id: str | None
    is_launched: bool
    is_pty_mode: bool
    is_background: bool
    background_name: str | None
    hcom_dir: Path
    hcom_dir_override: bool
    cwd: Path
    tool: ToolType
    claude_env_file: str | None
    stdin_is_tty: bool = True  # Default True for normal CLI usage
    stdout_is_tty: bool = True  # Default True for normal CLI usage
    # Tool markers for context-based detection (daemon-safe)
    is_claude: bool = False
    is_gemini: bool = False
    is_codex: bool = False
    hcom_go: bool = False
    # Codex thread ID (session equivalent) - available in all Codex child processes
    codex_thread_id: str | None = None
    # Launch context - who launched this instance and batch tracking
    launched_by: str | None = None
    launch_batch_id: str | None = None
    launch_event_id: str | None = None
    launched_preset: str | None = None
    notes: str = ""

    @classmethod
    def from_env(
        cls,
        env: dict[str, str],
        cwd: str | Path,
        *,
        stdin_is_tty: bool = True,
        stdout_is_tty: bool = True,
    ) -> "HcomContext":
        """Build context from explicit environment dict.

        Primary factory for daemon integration - env comes from request,
        not from os.environ.

        Args:
            env: Environment variables dict (e.g., from daemon request).
            cwd: Current working directory.
            stdin_is_tty: Client's stdin TTY status (for is_interactive).
            stdout_is_tty: Client's stdout TTY status (for is_interactive).

        Returns:
            Immutable HcomContext snapshot.
        """
        # Detect tool markers for context-based detection
        is_claude = env.get("CLAUDECODE") == "1" or bool(env.get("CLAUDE_ENV_FILE"))
        is_gemini = env.get("GEMINI_CLI") == "1"
        is_codex = (
            "CODEX_SANDBOX" in env
            or "CODEX_SANDBOX_NETWORK_DISABLED" in env
            or "CODEX_MANAGED_BY_NPM" in env
            or "CODEX_MANAGED_BY_BUN" in env
            or "CODEX_THREAD_ID" in env
        )
        hcom_go = env.get("HCOM_GO") == "1"

        # Determine tool type from markers
        tool: ToolType = "adhoc"
        if is_claude:
            tool = "claude"
        elif is_gemini:
            tool = "gemini"
        elif is_codex:
            tool = "codex"

        # Resolve hcom_dir (expand ~ for paths like ~/custom/.hcom)
        hcom_dir_str = env.get("HCOM_DIR")
        if hcom_dir_str:
            hcom_dir = Path(hcom_dir_str).expanduser()
        else:
            hcom_dir = Path.home() / ".hcom"

        return cls(
            process_id=env.get("HCOM_PROCESS_ID") or None,
            is_launched=env.get("HCOM_LAUNCHED") == "1",
            is_pty_mode=env.get("HCOM_PTY_MODE") == "1",
            is_background=bool(env.get("HCOM_BACKGROUND")),
            background_name=env.get("HCOM_BACKGROUND") or None,
            hcom_dir=hcom_dir,
            hcom_dir_override=bool(hcom_dir_str),
            cwd=Path(cwd) if isinstance(cwd, str) else cwd,
            tool=tool,
            claude_env_file=env.get("CLAUDE_ENV_FILE") or None,
            stdin_is_tty=stdin_is_tty,
            stdout_is_tty=stdout_is_tty,
            is_claude=is_claude,
            is_gemini=is_gemini,
            is_codex=is_codex,
            hcom_go=hcom_go,
            codex_thread_id=env.get("CODEX_THREAD_ID") or None,
            launched_by=env.get("HCOM_LAUNCHED_BY") or None,
            launch_batch_id=env.get("HCOM_LAUNCH_BATCH_ID") or None,
            launch_event_id=env.get("HCOM_LAUNCH_EVENT_ID") or None,
            launched_preset=env.get("HCOM_LAUNCHED_PRESET") or None,
            notes=env.get("HCOM_NOTES") or "",
        )

    @classmethod
    def from_os(cls) -> "HcomContext":
        """Build context from current os.environ and cwd.

        Convenience wrapper for non-daemon usage (CLI, direct hook calls).

        Returns:
            Immutable HcomContext snapshot of current environment.
        """
        return cls.from_env(dict(os.environ), Path.cwd())

    def with_tool(self, tool: ToolType) -> "HcomContext":
        """Create copy with different tool type.

        Useful for testing or when tool detection needs override.
        """
        return HcomContext(
            process_id=self.process_id,
            is_launched=self.is_launched,
            is_pty_mode=self.is_pty_mode,
            is_background=self.is_background,
            background_name=self.background_name,
            hcom_dir=self.hcom_dir,
            hcom_dir_override=self.hcom_dir_override,
            cwd=self.cwd,
            tool=tool,
            claude_env_file=self.claude_env_file,
            stdin_is_tty=self.stdin_is_tty,
            stdout_is_tty=self.stdout_is_tty,
            is_claude=self.is_claude,
            is_gemini=self.is_gemini,
            is_codex=self.is_codex,
            hcom_go=self.hcom_go,
            codex_thread_id=self.codex_thread_id,
            launched_by=self.launched_by,
            launch_batch_id=self.launch_batch_id,
            launch_event_id=self.launch_event_id,
            notes=self.notes,
        )

    # === Derived Properties ===

    @property
    def db_path(self) -> Path:
        """Path to hcom.db."""
        return self.hcom_dir / "hcom.db"

    @property
    def socket_path(self) -> Path:
        """Path to daemon socket."""
        return self.hcom_dir / "hcomd.sock"

    @property
    def log_dir(self) -> Path:
        """Path to logs directory."""
        return self.hcom_dir / ".tmp" / "logs"


__all__ = ["HcomContext", "ToolType"]
