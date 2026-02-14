"""Core package - fundamental system components for HCOM.

This package contains the foundational modules that power HCOM's multi-agent
communication system. These modules are designed to be independent and avoid
circular imports.

Architecture Overview
---------------------
The core package follows an events-first architecture where all state changes
are logged as events to SQLite. Key design principles:

- **Events-first**: All state changes (messages, status, lifecycle) are events
- **Three-tier identity**: process binding → session binding → ad-hoc
- **Row-exists = participating**: No enabled flags, presence in DB = active
- **TCP notifications**: Instant message wake via lightweight TCP pings

Module Guide
------------
db.py
    SQLite persistence layer. Schema v14 with WAL mode for concurrent access.
    Handles events, instances, bindings, and key-value storage.

identity.py
    Identity resolution from CLI args and environment. Resolves --name flags,
    agent IDs, and process bindings to canonical instance identities.

instances.py
    Instance lifecycle management. CVCV name generation, status tracking,
    binding resolution, and cleanup of stale instances.

messages.py
    Message routing and delivery. Scope-based filtering (broadcast/mentions),
    envelope fields (intent/thread/reply_to), and format helpers.

config.py
    Configuration with validation. Loads from env vars → config.toml → defaults.
    HcomConfig dataclass with comprehensive validation.

bootstrap.py
    Context injection for AI agents. Template-based bootstrap text generation
    with tool-specific sections (Claude/Gemini/Codex).

paths.py
    File system utilities. HCOM_DIR resolution, atomic writes, flag counters.

helpers.py
    Message routing validation. Scope/intent validation, @mention matching.

ops.py
    Clean operational API. High-level operations (send, stop, launch) that
    raise HcomError on failure and return meaningful data on success.

runtime.py
    Runtime utilities. Environment building, TCP notifications to instances.

log.py
    Structured JSONL logging with rotation. Logs to ~/.hcom/.tmp/logs/hcom.log.

context.py
    Launch context capture. Snapshots environment for disambiguation/audit.

device.py
    Device identity for relay. Persistent UUID with 4-char short ID.

tool_utils.py
    Cross-tool utilities. Permission generation, command building, instance
    stopping with proper cleanup.

transcript.py
    Conversation extraction. Multi-tool parsers (Claude/Gemini/Codex) for
    transcript analysis and timeline generation.

Usage
-----
Most external code should use the high-level ops.py functions or import
specific utilities:

    from hcom.core import resolve_identity
    from hcom.core.config import get_config
    from hcom.core.messages import send_message
    from hcom.core.ops import op_send, op_stop
"""

from .identity import resolve_identity
from .hcom_context import HcomContext, ToolType
from .hook_payload import HookPayload
from .hook_result import HookResult


__all__ = [
    "resolve_identity",
    # Context and payload types for daemon integration
    "HcomContext",
    "ToolType",
    "HookPayload",
    "HookResult",
]
