"""Hook utility functions and re-exports.

This module provides:
1. Hook context initialization (binding lookup, metadata extraction)
2. Transcript-based binding fallback for vanilla instances
3. Re-exports of commonly used functions from core modules

Key Functions:
    init_hook_context()         - Initialize instance context from hook_data
    _try_bind_from_transcript() - Fallback binding via transcript marker

Identity Resolution Flow:
    1. Look up session_id in session_bindings table
    2. If not found, check transcript for [hcom:X] marker
    3. If marker found and instance pending, create binding
    4. Return instance_name or None (non-participant)
"""

from __future__ import annotations

from pathlib import Path
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from ..core.hcom_context import HcomContext

from ..core.paths import hcom_path, LOGS_DIR
from ..core.log import log_info
from ..core.bootstrap import get_bootstrap


def find_last_bind_marker(transcript_path: str) -> str | None:
    """Find last [hcom:xxx] or [HCOM:BIND:xxx] marker in transcript using chunked backwards search.

    Reads file backwards in 64MB chunks to find marker efficiently.
    For 1.3GB file: ~0.08s if marker near end, ~1.6s if absent.

    Args:
        transcript_path: Path to transcript file

    Returns:
        Instance name from marker, or None if not found
    """
    from ..shared import BIND_MARKER_RE

    # Search for both new [hcom:name] and legacy [HCOM:BIND:name] formats
    marker_prefixes = [b"[hcom:", b"[HCOM:BIND:"]
    chunk_size = 64 * 1024 * 1024  # 64MB chunks
    overlap = 20 + 50  # max prefix len + max instance name

    try:
        file_path = Path(transcript_path)
        file_size = file_path.stat().st_size

        with open(file_path, "rb") as f:
            pos = file_size
            carry = b""

            while pos > 0:
                read_size = min(chunk_size, pos)
                pos -= read_size
                f.seek(pos)
                data = f.read(read_size)

                buf = data + carry
                # Find the last occurrence of any marker prefix
                best_idx = -1
                for prefix in marker_prefixes:
                    idx = buf.rfind(prefix)
                    if idx > best_idx:
                        best_idx = idx
                if best_idx != -1:
                    end_idx = buf.find(b"]", best_idx)
                    if end_idx != -1:
                        marker_bytes = buf[best_idx : end_idx + 1]
                        try:
                            marker_str = marker_bytes.decode("utf-8")
                            m = BIND_MARKER_RE.search(marker_str)
                            if m:
                                return m.group(1)
                        except UnicodeDecodeError:
                            pass

                carry = data[:overlap] if overlap > 0 else b""

    except Exception:
        pass

    return None


def _try_bind_from_transcript(session_id: str, transcript_path: str) -> str | None:
    """Check transcript for [hcom:X] marker and create session binding.

    Fallback binding mechanism for vanilla Claude instances that used the
    `!hcom start` bash shortcut (which bypasses PostToolUse hook detection).

    Args:
        session_id: Claude session ID from hook_data
        transcript_path: Path to Claude's JSONL transcript

    Returns:
        Instance name if binding created, None otherwise.

    Optimization:
        Skips file I/O if no pending instances exist (fast path).
    """
    log_info(
        "hooks",
        "transcript.bind.start",
        session_id=session_id,
        transcript_path=transcript_path,
    )

    if not transcript_path or not session_id:
        log_info(
            "hooks",
            "transcript.bind.skip",
            reason="missing session_id or transcript_path",
        )
        return None

    # Optimization: skip file I/O if no pending instances
    from ..core.db import get_pending_instances

    pending = get_pending_instances()
    if not pending:
        log_info("hooks", "transcript.bind.skip", reason="no pending instances")
        return None

    # Search backwards in chunks for marker (efficient for large files)
    instance_name = find_last_bind_marker(transcript_path)
    log_info("hooks", "transcript.bind.search", found=instance_name)

    if not instance_name:
        log_info("hooks", "transcript.bind.skip", reason="no marker matches")
        return None

    # Only bind if instance is in pending list (avoids binding to wrong session)
    if instance_name not in pending:
        log_info(
            "hooks",
            "transcript.bind.skip",
            reason="instance not pending",
            instance=instance_name,
            pending=pending,
        )
        return None

    from ..core.db import rebind_instance_session, get_instance
    from ..core.instances import update_instance_position

    instance = get_instance(instance_name)
    if not instance:
        log_info(
            "hooks",
            "transcript.bind.skip",
            reason="instance not found",
            instance=instance_name,
        )
        return None

    rebind_instance_session(instance_name, session_id)
    update_instance_position(instance_name, {"session_id": session_id})
    log_info("hooks", "transcript.bind.success", instance=instance_name)
    return instance_name


def inject_bootstrap_once(
    instance_name: str,
    instance_data: dict[str, Any],
    tool: str = "claude",
) -> str | None:
    """Inject bootstrap text if not already announced.

    Bootstrap text introduces the agent to hcom commands and capabilities.
    This function is idempotent - it checks the name_announced flag and only
    injects once per instance lifecycle.

    Args:
        instance_name: Instance identifier (e.g., "alice", "bob_general_1")
        instance_data: Instance position data containing name_announced flag.
                      Typically from load_instance_position(instance_name).
        tool: Tool type for bootstrap customization. One of: "claude", "gemini", "codex".
             Defaults to "claude".

    Returns:
        Bootstrap text string if injection is needed, None if already announced.

    Side Effects:
        Sets name_announced=True in instance position file if injection occurs.
        This prevents duplicate bootstrap injection on subsequent hook calls.

    Example:
        >>> instance = load_instance_position("alice")
        >>> if bootstrap := inject_bootstrap_once("alice", instance, tool="claude"):
        ...     print(bootstrap)  # First call returns bootstrap text
        >>> if bootstrap := inject_bootstrap_once("alice", instance, tool="claude"):
        ...     print("unreachable")  # Second call returns None (already announced)

    Design Notes:
        - Consolidates bootstrap injection logic from 5 locations across parent.py
          and gemini/hooks.py into a single helper.
        - name_announced flag is instance-scoped, not session-scoped. It persists
          across session rebinds but resets on instance restart.
        - Codex doesn't currently use hook-based bootstrap (PTY-only), so this
          helper is mainly for Claude and Gemini hooks.
    """
    from ..core.instances import update_instance_position

    if instance_data.get("name_announced", False):
        return None

    bootstrap = get_bootstrap(instance_name, tool=tool)
    update_instance_position(instance_name, {"name_announced": True})

    return bootstrap


def init_hook_context(
    ctx: "HcomContext",
    hook_data: dict[str, Any],
    hook_type: str | None = None,  # noqa: ARG001 - kept for API compatibility
) -> tuple[str | None, dict[str, Any], bool]:
    """Initialize instance context from hook_data via session binding lookup.

    Primary gate for hook participation: session must be bound to an instance
    via session_bindings table. If not bound, falls back to transcript marker.

    Args:
        ctx: Execution context with cwd and background info.
        hook_data: Claude hook payload containing session_id, transcript_path, etc.
        hook_type: Hook type string (unused, kept for API compatibility)

    Returns:
        Tuple of (instance_name, updates_dict, is_matched_resume):
        - instance_name: Resolved instance name, or None if not participating
        - updates_dict: Metadata updates to persist (directory, transcript_path, etc.)
        - is_matched_resume: True if session_id matches stored value (resume scenario)

    Gate Logic:
        1. Look up session_id in session_bindings → instance_name
        2. If not found, try transcript marker fallback
        3. If still not found → (None, {}, False) - non-participant
    """
    import time as _time
    from ..core.db import get_session_binding, get_instance

    start = _time.perf_counter()
    session_id = hook_data.get("session_id", "")
    transcript_path = hook_data.get("transcript_path", "")

    # Session binding is the sole gate for hook participation
    binding_start = _time.perf_counter()
    instance_name = get_session_binding(session_id)
    binding_ms = (_time.perf_counter() - binding_start) * 1000

    transcript_ms = 0.0
    if not instance_name:
        # Fallback: check transcript for binding marker (handles !hcom start)
        transcript_start = _time.perf_counter()
        instance_name = _try_bind_from_transcript(session_id, transcript_path)
        transcript_ms = (_time.perf_counter() - transcript_start) * 1000
        if not instance_name:
            total_ms = (_time.perf_counter() - start) * 1000
            log_info("hooks", "init_hook_context.timing",
                     binding_ms=round(binding_ms, 2),
                     transcript_ms=round(transcript_ms, 2),
                     total_ms=round(total_ms, 2),
                     result="no_instance")
            return None, {}, False

    instance_start = _time.perf_counter()
    instance_data = get_instance(instance_name)
    instance_ms = (_time.perf_counter() - instance_start) * 1000

    updates: dict[str, Any] = {
        "directory": str(ctx.cwd),
    }

    if transcript_path:
        updates["transcript_path"] = transcript_path

    if ctx.is_background and ctx.background_name:
        updates["background"] = True
        updates["background_log_file"] = str(hcom_path(LOGS_DIR, ctx.background_name))

    is_matched_resume = bool(instance_data and instance_data.get("session_id") == session_id)

    total_ms = (_time.perf_counter() - start) * 1000
    log_info("hooks", "init_hook_context.timing",
             instance=instance_name, binding_ms=round(binding_ms, 2),
             transcript_ms=round(transcript_ms, 2),
             instance_ms=round(instance_ms, 2),
             total_ms=round(total_ms, 2))

    return instance_name, updates, is_matched_resume
