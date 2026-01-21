"""Codex CLI integration for hcom.

Provides hook handlers, settings management, and runtime preprocessing
for OpenAI Codex CLI integration with hcom multi-agent communication.

Components:
    hooks.py: Single notify hook handler (codex-notify)
        - Called after each agent turn completes
        - Updates instance status to listening
        - Handles vanilla instance binding via transcript marker

    settings.py: Config file management (~/.codex/config.toml)
        - Notify hook installation/removal
        - Execpolicy rules for auto-approving safe hcom commands

    args.py: CLI argument parsing (CodexArgsSpec)
        - Subcommands (exec, resume, fork, review)
        - Flag validation and merging
        - Semantic flags: is_exec (rejected), is_json

    preprocessing.py: Runtime argument preprocessing
        - Sandbox flags injection (workspace-write mode)
        - --add-dir ~/.hcom for hcom DB access in sandbox
        - Bootstrap injection via developer_instructions

    transcript.py: Transcript parsing for event detection
        - Watches rollout-*.jsonl for tool calls
        - Logs file edits (apply_patch) for collision detection
        - Logs shell commands for cmd: subscriptions

Architecture Note:
    Codex has only one hook (notify) unlike Claude/Gemini's multiple hooks.
    Message delivery uses PTY injection via TranscriptWatcher detecting idle,
    not hook-based injection like Gemini.
"""

from .hooks import handle_codex_hook
from .settings import (
    setup_codex_hooks,
    verify_codex_hooks_installed,
    remove_codex_hooks,
)
from .preprocessing import (
    preprocess_codex_args,
    get_sandbox_flags,
    ensure_hcom_writable,
    add_codex_developer_instructions,
)
from .transcript import (
    TranscriptWatcher,
    run_transcript_watcher_thread,
)

__all__ = [
    "handle_codex_hook",
    "setup_codex_hooks",
    "verify_codex_hooks_installed",
    "remove_codex_hooks",
    # Preprocessing (codex-specific runtime stuff)
    "preprocess_codex_args",
    "get_sandbox_flags",
    "ensure_hcom_writable",
    "add_codex_developer_instructions",
    # Transcript watcher
    "TranscriptWatcher",
    "run_transcript_watcher_thread",
]
