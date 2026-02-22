"""Transcript path discovery for Claude, Gemini, Codex, and OpenCode."""

from __future__ import annotations

import glob
import os
from pathlib import Path


def get_claude_config_dir() -> Path:
    """Get Claude config directory, respecting CLAUDE_CONFIG_DIR env var.

    Returns:
        Path to Claude config directory (default: ~/.claude)
    """
    claude_dir = os.environ.get("CLAUDE_CONFIG_DIR")
    if claude_dir:
        return Path(claude_dir)
    return Path.home() / ".claude"


def derive_gemini_transcript_path(session_id: str | None) -> str | None:
    """Derive Gemini CLI transcript path from session_id.

    Gemini's ChatRecordingService isn't initialized at SessionStart, so the
    transcript_path field is empty. This function derives it from the session_id
    by searching the Gemini chats directory.

    Args:
        session_id: Gemini session ID (format: prefix-uuid)

    Returns:
        Full path to transcript file if found, None otherwise

    Search Strategy:
        - Extract prefix from session_id (everything before first hyphen)
        - Search ~/.gemini/tmp/**/chats/ for session-*-{prefix}*.json
        - Return most recently modified match

    Example:
        >>> derive_gemini_transcript_path("abc123-uuid-here")
        '/Users/user/.gemini/tmp/project/chats/session-1-abc123-rest.json'
    """
    if not session_id:
        return None

    try:
        session_prefix = session_id.split("-")[0]
        gemini_chats = Path.home() / ".gemini" / "tmp"
        pattern = str(gemini_chats / "**" / "chats" / f"session-*-{session_prefix}*.json")
        matches = glob.glob(pattern, recursive=True)
        if matches:
            # Return most recently modified
            return max(matches, key=lambda p: Path(p).stat().st_mtime)
    except Exception:
        pass

    return None


def get_opencode_db_path() -> str | None:
    """Get path to OpenCode's SQLite database.

    OpenCode uses XDG_DATA_HOME/opencode/opencode.db.
    Default XDG_DATA_HOME is ~/.local/share on Linux/macOS.
    """
    xdg_data = os.environ.get("XDG_DATA_HOME") or str(Path.home() / ".local" / "share")
    db_path = Path(xdg_data) / "opencode" / "opencode.db"
    if db_path.exists():
        return str(db_path)
    return None


def derive_codex_transcript_path(thread_id: str | None) -> str | None:
    """Derive Codex CLI transcript path from thread_id.

    Searches the Codex sessions directory for rollout files matching the thread_id.
    Respects CODEX_HOME environment variable if set.

    Args:
        thread_id: Codex thread ID (UUID format)

    Returns:
        Full path to transcript file if found, None otherwise

    Search Strategy:
        - Use $CODEX_HOME/sessions if set, else ~/.codex/sessions
        - Search for rollout-*-{thread_id}.jsonl recursively
        - Return most recently modified match (deterministic selection)

    Example:
        >>> derive_codex_transcript_path("abc-123-def")
        '/Users/user/.codex/sessions/project/rollout-1-abc-123-def.jsonl'
    """
    if not thread_id:
        return None

    try:
        # Respect CODEX_HOME env var if set, else default to ~/.codex
        codex_base = os.environ.get("CODEX_HOME") or str(Path.home() / ".codex")
        codex_home = Path(codex_base) / "sessions"
        pattern = str(codex_home / "**" / f"rollout-*-{thread_id}.jsonl")
        matches = glob.glob(pattern, recursive=True)
        if matches:
            # Return most recently modified for deterministic selection
            return max(matches, key=lambda p: Path(p).stat().st_mtime)
    except Exception:
        pass

    return None
