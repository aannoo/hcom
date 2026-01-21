"""Codex CLI integration.

Thin wrapper around unified pty_handler functions.

Bootstrap delivery:
- Full bootstrap injected at launch via `-c developer_instructions=...` flag
- Built via `get_bootstrap()` in `preprocess_codex_args()`

Message delivery (listen-push):
- When Codex is safe for input, inject a short instruction that causes Codex
  to run `hcom listen` in its own tool environment.

Transcript parsing:
- TranscriptWatcher periodically parses Codex transcript for file edits (apply_patch)
- Logs status events with original timestamps for collision detection
"""

from __future__ import annotations

import os
import sys
import threading

from .pty_handler import run_pty_with_hcom
from ..tools.codex.transcript import TranscriptWatcher, run_transcript_watcher_thread
from ..core.log import log_error


# ==================== PTY Runner ====================


def run_codex_with_hcom(
    tool: str,  # Always "codex", ignored (unified signature compatibility)
    instance_name: str,
    codex_args: list[str] | None = None,
    *,
    resume_thread_id: str | None = None,
) -> int:
    """Run Codex with hcom listen-push integration. Blocks until Codex exits.

    Reads HCOM_PROCESS_ID from env (set by launcher).
    """
    del tool  # Unused, exists for unified signature compatibility
    from ..tools.codex.preprocessing import preprocess_codex_args

    process_id = os.environ.get("HCOM_PROCESS_ID")

    # Resolve instance name from process binding if available
    if process_id:
        try:
            from ..core.db import get_process_binding

            binding = get_process_binding(process_id)
            bound_name = binding.get("instance_name") if binding else None
            if bound_name:
                instance_name = bound_name
        except Exception:
            pass

    # Preprocess args: sandbox flags, --add-dir, bootstrap injection
    sandbox_mode = os.environ.get("HCOM_CODEX_SANDBOX_MODE", "workspace")
    codex_args = preprocess_codex_args(
        codex_args or [],
        instance_name,
        sandbox_mode=sandbox_mode,
    )

    # Handle resume: bind session to process
    if resume_thread_id:
        try:
            from ..core.instances import bind_session_to_process

            canonical = bind_session_to_process(resume_thread_id, process_id)
            if canonical and canonical != instance_name:
                instance_name = canonical
        except Exception as e:
            log_error("pty", "status.change", e, instance=instance_name, tool="codex")

    # Required for Codex notify hook handler to activate
    os.environ["HCOM_LAUNCHED"] = "1"

    if not process_id:
        print("[codex-pty] ERROR: HCOM_PROCESS_ID not set", file=sys.stderr)
        return 1

    if not instance_name:
        print("[codex-pty] ERROR: instance_name not set", file=sys.stderr)
        return 1

    # Create transcript watcher for file edit status tracking
    transcript_watcher = TranscriptWatcher(instance_name)

    def on_ready_start_watcher(inst_name: str, running_flag: list[bool]) -> None:
        """Start transcript watcher when PTY is ready."""
        from ..core.instances import set_status

        # Set initial listening status
        set_status(inst_name, "listening", "start")

        # Start transcript watcher thread
        threading.Thread(
            target=run_transcript_watcher_thread,
            kwargs={
                "instance_name": inst_name,
                "process_id": process_id,
                "watcher": transcript_watcher,
                "running_flag": running_flag,
                "poll_interval": 5.0,
            },
            daemon=True,
        ).start()

    return run_pty_with_hcom(
        "codex",
        instance_name,
        codex_args,
        on_ready_extra=on_ready_start_watcher,
    )


__all__ = [
    "run_codex_with_hcom",
]
