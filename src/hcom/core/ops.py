"""Core operations for HCOM.

Clean operational layer used by both CLI commands and Python API.
Raises HcomError on failure, returns meaningful data on success.
"""

from __future__ import annotations

from ..shared import HcomError, SenderIdentity


def op_send(
    identity: SenderIdentity, message: str, envelope: dict[str, str] | None = None
) -> list[str]:
    """Send message.

    Args:
        identity: Sender identity (from resolve_identity or constructed)
        message: Message text (can include @mentions)
        envelope: Optional envelope fields {intent, reply_to, thread}

    Returns:
        List of instance names message was delivered to

    Raises:
        HcomError: If validation fails or delivery fails
    """
    from .messages import send_message

    return send_message(identity, message, envelope=envelope)


def op_stop(
    instance_name: str, initiated_by: str | None = None, reason: str = "api"
) -> None:
    """Stop an instance (deletes row).

    Args:
        instance_name: Instance to stop
        initiated_by: Who initiated the stop (for logging)
        reason: Reason for stop (for logging)

    Raises:
        HcomError: If instance not found or is remote
    """
    from .instances import load_instance_position
    from .tool_utils import stop_instance

    position = load_instance_position(instance_name)
    if not position:
        raise HcomError(f"Instance '{instance_name}' not found")

    if position.get("origin_device_id"):
        raise HcomError(
            f"Cannot stop remote instance '{instance_name}' via ops - use relay"
        )

    stop_instance(instance_name, initiated_by=initiated_by, reason=reason)


def op_start(
    instance_name: str, initiated_by: str | None = None, reason: str = "api"
) -> None:
    """Start an instance.

    With row-exists=participating model, stopped instances are deleted and
    cannot be restarted via this API. Use 'hcom start' command instead.

    Args:
        instance_name: Instance to start
        initiated_by: Who initiated the start (for logging)
        reason: Reason for start (for logging)

    Raises:
        HcomError: If instance not found or is remote
    """
    from .instances import load_instance_position

    position = load_instance_position(instance_name)
    if not position:
        raise HcomError(
            f"Instance '{instance_name}' not found. Stopped instances are deleted - use 'hcom start' to create a new one."
        )

    if position.get("origin_device_id"):
        raise HcomError(
            f"Cannot start remote instance '{instance_name}' via ops - use relay"
        )

    # Row exists = already participating, nothing to do


def op_launch(
    count: int,
    claude_args: list[str],
    *,
    launcher: str,
    tag: str | None = None,
    background: bool = False,
    cwd: str | None = None,
    pty: bool = False,
) -> dict:
    """Launch Claude instances.

    Args:
        count: Number of instances to launch (1-100)
        claude_args: Claude CLI arguments (already parsed/merged)
        launcher: Name of launching instance (for logging)
        tag: HCOM_TAG value
        background: Headless mode
        cwd: Working directory for instances
        pty: Use PTY mode instead of native hooks

    Returns:
        {
            "batch_id": str,
            "launched": int,
            "failed": int,
            "background": bool,
            "log_files": list[str],
        }

    Raises:
        HcomError: If validation fails or no instances launched
    """
    from ..launcher import launch as unified_launch

    result = unified_launch(
        "claude",
        count,
        claude_args,
        tag=tag,
        background=background,
        cwd=cwd,
        launcher=launcher,
        pty=pty,
    )

    # Preserve existing return shape for callers.
    return {
        "batch_id": result["batch_id"],
        "launched": result["launched"],
        "failed": result["failed"],
        "background": result["background"],
        "log_files": result.get("log_files", []),
    }


def auto_subscribe_defaults(instance_name: str, tool: str) -> None:
    """Auto-subscribe instance to default event subscriptions from config.

    Called during instance creation. Only subscribes if tool supports collision
    detection (claude, gemini, codex). Cleans up any stale subscriptions first
    (from previously stopped instances with reused names).
    """
    if tool not in ("claude", "gemini", "codex"):
        return

    try:
        from .config import get_config
        from .db import get_db
        from ..commands.events import _events_sub

        # Clean up stale subscriptions for this instance name (from reused names)
        # Escape _ and % in instance name (they're LIKE wildcards)
        escaped_name = instance_name.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_")
        conn = get_db()
        conn.execute(
            "DELETE FROM kv WHERE key LIKE ? ESCAPE '\\'", (f"events_sub:%\\_{escaped_name}",)
        )

        config = get_config()
        if not config.auto_subscribe:
            return

        presets = [
            p.strip() for p in config.auto_subscribe.split(",") if p.strip()
        ]

        # Map old preset names to new composable filter flags
        preset_to_flags = {
            "collision": ["--collision"],
            "created": ["--action", "created"],
            "stopped": ["--action", "stopped"],
            "blocked": ["--status", "blocked"],
        }

        for preset in presets:
            try:
                flags = preset_to_flags.get(preset)
                if not flags:
                    # Unknown preset - skip
                    continue
                _events_sub(flags, caller_name=instance_name, silent=True)
            except Exception:
                pass
    except Exception:
        pass


__all__ = ["op_send", "op_stop", "op_start", "op_launch", "auto_subscribe_defaults"]
