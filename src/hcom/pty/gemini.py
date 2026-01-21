"""Gemini CLI integration.

Thin wrapper around unified pty_handler functions.
Key behavior:
- PTY wrapper injects message preview.
- Actual message content is delivered by Gemini's BeforeAgent hook.
"""

from __future__ import annotations

from .pty_handler import run_pty_with_hcom


# ==================== Args Resolution ====================

# Re-export from tools/gemini/args (canonical location)
from ..tools.gemini.args import (
    GeminiArgsSpec,
    resolve_gemini_args,
    merge_gemini_args,
)


def get_resolved_gemini_args(cli_args: list[str] | None = None) -> GeminiArgsSpec:
    """Resolve Gemini args with config precedence: CLI > env > config file.

    Merges HCOM_GEMINI_ARGS from config.env with CLI args.
    Validates for conflicts and returns parsed spec.
    """
    from ..core.config import get_config

    config = get_config()
    env_value = config.gemini_args if config.gemini_args else None

    # Parse env and CLI separately
    env_spec = resolve_gemini_args(None, env_value)
    cli_spec = resolve_gemini_args(cli_args, None)

    # Merge: CLI takes precedence
    if cli_args:
        return merge_gemini_args(env_spec, cli_spec)
    return env_spec


# ==================== PTY Runner ====================


def run_gemini_with_hcom(gemini_args: list[str] | None = None) -> int:
    """Run Gemini with hcom integration. Blocks until Gemini exits.

    Requires:
    - HCOM_PROCESS_ID set (provided by runner script / launcher)
    - Gemini hooks installed (BeforeAgent/AfterAgent) for content delivery + idle detection

    Instance name is resolved from process binding by run_pty_with_hcom.
    """
    # run_pty_with_hcom resolves instance_name from process binding
    return run_pty_with_hcom("gemini", "", list(gemini_args or []))


__all__ = [
    "run_gemini_with_hcom",
    "get_resolved_gemini_args",
    # Re-exports
    "GeminiArgsSpec",
    "resolve_gemini_args",
    "merge_gemini_args",
]
