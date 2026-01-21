"""Identity resolution - single source of truth for all tools.

Identity Model:
- HCOM-launched: HCOM_PROCESS_ID env var set by launcher
- Vanilla: Use --name <name> on all hcom commands
- Adhoc: hcom start creates instance, outputs name to use
- Task context: Subagent uses --name <agent_id>, parent uses --name <instance_name> (no auto-detect)

--name NAME: strict instance lookup (instance name or UUID)
--from NAME: external sender (send command only)
-b: alias for --from bigboss (send command only)
"""

from __future__ import annotations
import os
import re

from .db import get_db
from .instances import load_instance_position, resolve_instance_name
from ..shared import SenderIdentity, HcomError


# UUID pattern for agent_id detection
_UUID_PATTERN = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$", re.IGNORECASE)
_BASE_NAME_RE = re.compile(r"^[a-z0-9_]+$")


def _looks_like_uuid(value: str) -> bool:
    """Check if value looks like a UUID (agent_id format)."""
    return bool(_UUID_PATTERN.match(value))


def instance_not_found_error(name: str) -> str:
    """Generate actionable error message for instance not found."""
    return f"Instance '{name}' not found. Run 'hcom start --as {name}' to reclaim your identity."


def is_valid_base_name(name: str) -> bool:
    """Check if name is a valid base instance name."""
    return bool(_BASE_NAME_RE.match(name))


def base_name_error(name: str) -> str:
    """Build a consistent error for invalid base instance names."""
    return f"Invalid instance name '{name}'. Use base name only (lowercase letters, numbers, underscore)."


# Dangerous characters for user-provided names (injection prevention + naming consistency)
_DANGEROUS_CHARS_PATTERN = re.compile(r"[|&;$`<>]")
_DANGEROUS_CHARS_WITH_AT_PATTERN = re.compile(r"[|&;$`<>@]")


def validate_name_input(name: str, *, max_length: int = 50, allow_at: bool = True) -> str | None:
    """Validate user-provided name input for length and dangerous characters.

    Used for --name and --from flag validation in CLI commands.

    Args:
        name: User-provided name string
        max_length: Maximum allowed length (default 50)
        allow_at: If False, reject @ character (for --from validation)

    Returns:
        Error message string if invalid, None if valid.
    """
    if len(name) > max_length:
        return f"Name too long ({len(name)} chars, max {max_length})"

    pattern = _DANGEROUS_CHARS_PATTERN if allow_at else _DANGEROUS_CHARS_WITH_AT_PATTERN
    bad_chars = pattern.findall(name)
    if bad_chars:
        return f"Name contains invalid characters: {' '.join(set(bad_chars))}"

    return None


def resolve_from_name(name: str) -> SenderIdentity:
    """Resolve --name NAME with strict instance lookup.

    Resolution order:
    1. Instance name lookup (exact) → kind='instance' if found
    2. Agent ID (UUID) lookup → kind='instance' if found
    3. Error if not found (no external fallback)

    Args:
        name: The value from --name flag (base name only, no tag/device suffix)

    Returns:
        SenderIdentity with kind='instance'

    Raises:
        HcomError: If instance not found
    """
    # Reject invalid base names (tag/device suffixes are not allowed here)
    if not _looks_like_uuid(name) and not is_valid_base_name(name):
        raise HcomError(base_name_error(name))

    # 1. Instance name lookup (exact match)
    data = load_instance_position(name)
    if data:
        return SenderIdentity(
            kind="instance",
            name=name,
            instance_data=data,
            session_id=data.get("session_id"),
        )

    # 2. Agent ID lookup (Claude Code sends short IDs like 'a6d9caf')
    conn = get_db()
    row = conn.execute("SELECT name FROM instances WHERE agent_id = ?", (name,)).fetchone()
    if row:
        instance_name = row["name"]
        instance_data = load_instance_position(instance_name)
        if instance_data:
            return SenderIdentity(
                kind="instance",
                name=instance_name,
                instance_data=instance_data,
                session_id=instance_data.get("session_id"),
            )

    # 3. Not found - use central error helper
    raise HcomError(instance_not_found_error(name))


def resolve_identity(
    name: str | None = None,
    system_sender: str | None = None,
    session_id: str | None = None,
) -> SenderIdentity:
    """Resolve sender identity for CLI commands.

    Args:
        name: Instance name from --name flag (strict lookup, error if not found)
        system_sender: System notification sender name (e.g., 'hcom-launcher')
        session_id: Explicit session_id (for hook context, bypasses env detection)

    Priority:
    1. system_sender - system notifications
    2. session_id - explicit session (internal use)
    3. name (--name) - strict instance lookup
    4. Auto-detect from HCOM_PROCESS_ID
    5. Error if no identity

    Returns:
        SenderIdentity with kind, name, and optional instance_data/session_id

    Raises:
        HcomError: If identity required but not resolvable
    """
    # 1. System sender (internal use)
    if system_sender:
        return SenderIdentity(kind="system", name=system_sender)

    # 2. Explicit session_id (internal use)
    if session_id:
        resolved_name, data = resolve_instance_name(session_id)
        if not resolved_name or not data:
            raise HcomError("Instance not found for session_id")
        return SenderIdentity(
            kind="instance",
            name=resolved_name,
            instance_data=data,
            session_id=session_id,
        )

    # 3. Strict instance lookup (--name NAME)
    if name:
        return resolve_from_name(name)

    # 5. Auto-detect from process binding (hcom-launched instances)
    process_id = os.environ.get("HCOM_PROCESS_ID")
    if process_id:
        from .instances import resolve_process_binding

        bound_name = resolve_process_binding(process_id)
        if not bound_name:
            raise HcomError("Session expired. Run 'hcom start' to reconnect.")
        bound_data = load_instance_position(bound_name)
        if not bound_data:
            raise HcomError(instance_not_found_error(bound_name))
        # Row exists = participating
        return SenderIdentity(
            kind="instance",
            name=bound_name,
            instance_data=bound_data,
            session_id=bound_data.get("session_id"),
        )

    # 6. No identity - provide actionable guidance
    raise HcomError("No hcom identity. Run 'hcom start' first, then use --name <yourname> on commands.")


__all__ = [
    "_looks_like_uuid",
    "base_name_error",
    "instance_not_found_error",
    "is_valid_base_name",
    "resolve_from_name",
    "resolve_identity",
    "validate_name_input",
]
