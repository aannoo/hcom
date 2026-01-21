"""Gemini CLI integration for hcom.

Hook handlers for Gemini CLI's hook system:
- SessionStart: Identity setup
- BeforeAgent: Message delivery on prompt submit
- AfterTool: Message delivery during tool use
- BeforeTool: Status tracking
- SessionEnd: Mark instance inactive on exit

See .davia/assets/ for architecture details.
"""

from .hooks import (
    handle_gemini_hook,
    handle_sessionstart,
    handle_beforeagent,
    handle_aftertool,
    handle_beforetool,
    handle_sessionend,
    GEMINI_HOOK_HANDLERS,
)
from .settings import (
    setup_gemini_hooks,
    verify_gemini_hooks_installed,
    remove_gemini_hooks,
    get_gemini_settings_path,
    load_gemini_settings,
    GEMINI_HOOK_CONFIGS,
    GEMINI_HOOK_TYPES,
)
__all__ = [
    # Hook handlers
    "handle_gemini_hook",
    "handle_sessionstart",
    "handle_beforeagent",
    "handle_aftertool",
    "handle_beforetool",
    "handle_sessionend",
    "GEMINI_HOOK_HANDLERS",
    # Settings management
    "setup_gemini_hooks",
    "verify_gemini_hooks_installed",
    "remove_gemini_hooks",
    "get_gemini_settings_path",
    "load_gemini_settings",
    "GEMINI_HOOK_CONFIGS",
    "GEMINI_HOOK_TYPES",
]
