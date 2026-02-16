"""Message operations - filtering, routing, and delivery"""

from __future__ import annotations
from typing import Any, Literal, TypedDict
import re
import sys

from .instances import load_instance_position, update_instance_position
from .config import get_config
from ..shared import (
    MENTION_PATTERN,
    SENDER,
    SenderIdentity,
    HcomError,
    parse_iso_timestamp,
    format_age,
    MAX_MESSAGE_SIZE,
)
from .helpers import validate_scope, is_mentioned


# ==================== Type Definitions ====================

# Message scope types
MessageScope = Literal["broadcast", "mentions"]
MessageIntent = Literal["request", "inform", "ack"]


class MessageEnvelope(TypedDict, total=False):
    """Optional envelope fields for messages."""

    intent: MessageIntent
    reply_to: str  # "42" or "42:BOXE" for cross-device
    thread: str
    bundle_id: str


class RelayMetadata(TypedDict, total=False):
    """Relay metadata for cross-device messages."""

    id: int | str
    short: str  # Short device ID like "BOXE"


class MessageEventData(TypedDict, total=False):
    """Data stored in message events."""

    # Required fields
    scope: MessageScope
    text: str

    # Sender info (stored as 'from' but using underscore here)
    # Note: actual key is 'from' but that's a Python keyword

    # Routing fields
    sender_kind: Literal["external", "instance", "system"]
    delivered_to: list[str]  # Base names of recipients
    mentions: list[str]  # For scope='mentions'

    # Envelope fields
    intent: MessageIntent
    reply_to: str
    reply_to_local: int  # Resolved local event ID
    thread: str
    bundle_id: str

    # Relay metadata
    _relay: RelayMetadata


class UnreadMessage(TypedDict):
    """Message returned by get_unread_messages."""

    timestamp: str  # ISO timestamp
    event_id: int
    message: str  # Text content

    # Note: 'from' is a reserved keyword, so we use it dynamically


class FormattedMessage(TypedDict, total=False):
    """Fully formatted message with all optional fields."""

    timestamp: str
    event_id: int
    message: str
    delivered_to: list[str]
    intent: MessageIntent
    thread: str
    bundle_id: str
    _relay: RelayMetadata


class ReadReceipt(TypedDict):
    """Read receipt for a sent message."""

    id: int
    age: str  # Formatted age string like "2m" or "1h"
    text: str  # Truncated message text
    read_by: list[str]  # Instance names that have read
    total_recipients: int


# Scope computation result types
ScopeExtra = dict[str, list[str]]  # e.g., {"mentions": ["luna", "nova"]}
ScopeResult = tuple[MessageScope, ScopeExtra]


def _warn_missing_scope(event_id: int) -> None:
    print(f"Error: Message event {event_id} missing 'scope' field (old format). Run 'hcom reset logs' to clear old messages.", file=sys.stderr)


def _warn_corrupt_message(event_id: int, error: Exception) -> None:
    print(f"Error: Corrupt message data in event {event_id}: {error}. Run 'hcom reset logs' to clear corrupt messages.", file=sys.stderr)


def validate_message(message: str) -> str | None:
    """Validate message content and size.

    Args:
        message: Message text to validate.

    Returns:
        Error message string if invalid, None if valid.
    """
    if not message or not message.strip():
        return "Message required"

    # Reject control characters (except \n, \r, \t)
    if re.search(r"[\x00-\x08\x0B-\x0C\x0E-\x1F\u0080-\u009F]", message):
        return "Message contains control characters"

    if len(message) > MAX_MESSAGE_SIZE:
        return f"Message too large (max {MAX_MESSAGE_SIZE} chars)"

    return None


# ==================== Formatting Helpers ====================


def format_recipients(delivered_to: list[str], max_show: int = 30) -> str:
    """Format recipients list for display.

    Args:
        delivered_to: Instances that received the message (enabled at send time)
        max_show: Max names to show before truncating (default: 30)

    Returns:
        "luna, nova" or "luna, nova, kira (+2 more)" or "(none)"
    """
    if not delivered_to:
        return "(none)"

    if len(delivered_to) > max_show:
        shown = ", ".join(delivered_to[:max_show])
        remaining = len(delivered_to) - max_show
        return f"{shown} (+{remaining} more)"

    return ", ".join(delivered_to)


# ==================== Scope Computation ====================


def _match_target(target: str, full_names: list[str], full_to_base: dict[str, str]) -> list[str]:
    """Match a target against instance names, with base-name fallback.

    Tries prefix match on full display name first ({tag}-{name}).
    Falls back to prefix match on base name ({name} only) if no full-name match.

    Special case: bigboss:SUFFIX resolves to bigboss (virtual identity, device-agnostic).
    """
    # bigboss is device-agnostic - strip any remote suffix
    if target.lower().startswith("bigboss:"):
        if "bigboss" in full_names or "bigboss" in full_to_base.values():
            return ["bigboss"]
        return []

    if ":" in target:
        # Remote target - match any instance with prefix
        return [full_to_base[fn] for fn in full_names if fn.lower().startswith(target.lower())]

    # Local target - prefix match on full display name
    matches = [
        full_to_base[fn]
        for fn in full_names
        if ":" not in fn
        and fn.lower().startswith(target.lower())
        and (len(fn) == len(target) or fn[len(target)] != "_")
    ]
    if matches:
        return matches

    # Fallback: prefix match on base name (ignoring tag prefix)
    return [
        full_to_base[fn]
        for fn in full_names
        if ":" not in fn
        and full_to_base[fn].lower().startswith(target.lower())
        and (len(full_to_base[fn]) == len(target) or full_to_base[fn][len(target)] != "_")
    ]


def compute_scope(
    message: str,
    enabled_instances: list[dict[str, Any] | str],
    explicit_targets: list[str] | None = None,
) -> tuple[ScopeResult | None, str | None]:
    """Compute message scope and routing data.

    Args:
        message: Message text
        enabled_instances: List of enabled instance dicts (with 'name' and 'tag' fields)
        explicit_targets: Optional list of explicit target names (from CLI @args).
                         If provided, these are used for routing instead of parsing
                         @mentions from message text.

    Returns:
        ((scope, extra_data), None) on success
        (None, error_message) on validation failure

    Scope types:
        - 'broadcast': No targets → everyone
        - 'mentions': Has targets → explicit targets only

    STRICT FAILURE: Targets that don't match enabled instances return error

    Target matching uses full display name ({tag}-{name} or {name}):
        - api-luna matches instance with tag='api', name='luna'
        - api- matches all instances with tag='api' (prefix match)
        - luna matches instance with name='luna' (no tag, or base-name fallback)
    """
    from .instances import get_full_name

    # Build full name lookup: {full_name: base_name}
    # We store base_name in mentions list (that's the PK) but match against full_name
    full_to_base = {}
    full_names = []
    for inst in enabled_instances:
        if isinstance(inst, str):
            # Backwards compat: plain string (base name or legacy full name)
            full_to_base[inst] = inst
            full_names.append(inst)
        else:
            full = get_full_name(inst)
            base = inst.get("name", "")
            full_to_base[full] = base
            full_names.append(full)

    # If explicit targets specified (via -- separator), use them instead of parsing @mentions
    # Note: empty list means broadcast was explicitly requested (no @targets before --)
    # None means fall back to @mention parsing in message text (old syntax)
    if explicit_targets is not None:
        if explicit_targets:
            # Has explicit targets - route to them
            matched_base_names = []
            unmatched = []

            for target in explicit_targets:
                matches = _match_target(target, full_names, full_to_base)
                if matches:
                    matched_base_names.extend(matches)
                else:
                    unmatched.append(target)

            if unmatched:
                display = format_recipients(full_names)
                error = (
                    f"Unknown targets: {', '.join(f'@{t}' for t in unmatched)}\n"
                    f"Available: {display}\n"
                    f"Tip: @api- matches tag group | @luna matches prefix | underscore blocks (@luna won't match luna_sub)"
                )
                return None, error

            unique_instances = list(dict.fromkeys(matched_base_names))
            if unique_instances:
                return ("mentions", {"mentions": unique_instances}), None

        # Empty explicit_targets or no matches = broadcast
        return ("broadcast", {}), None

    # No explicit targets (None) - check for @mentions in message text (backward compat)
    if "@" in message:
        # First check for invalid system notification mention attempts like @[hcom-events]
        # These don't match MENTION_PATTERN but would broadcast to everyone
        system_bracket_pattern = re.compile(r"@\[hcom-[a-z]+\]")
        system_bracket_attempts = system_bracket_pattern.findall(message)
        if system_bracket_attempts:
            return (
                None,
                f"System notifications cannot be mentioned: {', '.join(system_bracket_attempts)}\nSystem notifications (names in []) are not agents and cannot receive messages.",
            )

        mentions = MENTION_PATTERN.findall(message)
        if mentions:
            # Validate all mentions match ENABLED instances only
            matched_base_names = []
            unmatched = []

            for mention in mentions:
                matches = _match_target(mention, full_names, full_to_base)
                if matches:
                    matched_base_names.extend(matches)
                else:
                    unmatched.append(mention)

            # STRICT: fail on unmatched mentions (non-existent OR disabled)
            if unmatched:
                # Special cases: literal "@mention", "@name", or "@mentions" in message text
                special_literals = {"mention", "name", "mentions"}
                literal_matches = [m for m in unmatched if m in special_literals]
                if literal_matches:
                    if len(literal_matches) == 1:
                        literal_text = f"@{literal_matches[0]}"
                    else:
                        literal_text = ", ".join(f"@{m}" for m in literal_matches)
                    return (
                        None,
                        f"The literal text {literal_text} is not a valid target - use actual instance names",
                    )

                display = format_recipients(full_names)
                error = (
                    f"@mentions to non-existent or stopped agents (or you used '@' char for stuff that wasn't agent name): "
                    f"{', '.join(f'@{m}' for m in unmatched)}\nAvailable: {display}"
                )
                return None, error

            # Deduplicate matched instances (store base names for DB lookup)
            unique_instances = list(dict.fromkeys(matched_base_names))
            return ("mentions", {"mentions": unique_instances}), None

    # No @mentions → broadcast to everyone
    return ("broadcast", {}), None



# ==================== Core Message Operations ====================


def resolve_reply_to(reply_to: str) -> tuple[int | None, str | None]:
    """Resolve reply_to reference to local event ID.

    Handles both local (42) and cross-device (42:BOXE) formats.

    Args:
        reply_to: Event reference string - "42" or "42:BOXE"

    Returns:
        (local_event_id, warning_message)
        - local_event_id: Resolved local event ID, or None if can't resolve
        - warning_message: Warning string if resolution failed, else None
    """
    from .db import get_db

    conn = get_db()

    if ":" in reply_to:
        # Cross-device format: 42:BOXE
        parts = reply_to.split(":", 1)
        try:
            remote_id = int(parts[0])
            short_device = parts[1].upper()
        except (ValueError, IndexError):
            return None, f"Invalid reply_to format: {reply_to}"

        # Look up by relay origin metadata (must be a message event)
        row = conn.execute(
            """
            SELECT id FROM events
            WHERE type = 'message'
              AND json_extract(data, '$._relay.short') = ?
              AND (
                json_extract(data, '$._relay.id') = ?
                OR json_extract(data, '$._relay.id') = ?
              )
            ORDER BY id DESC
            LIMIT 1
            """,
            (short_device, remote_id, str(remote_id)),
        ).fetchone()

        if row:
            return row["id"], None
        return None, f"Remote event {reply_to} not found locally"
    else:
        # Local format: 42
        try:
            local_id = int(reply_to)
        except ValueError:
            return None, f"Invalid reply_to format: {reply_to}"

        # Verify event exists and is a message (not status/life events)
        row = conn.execute("SELECT id FROM events WHERE id = ? AND type = 'message'", (local_id,)).fetchone()
        if row:
            return local_id, None
        return None, f"Message #{reply_to} not found"


def _get_event_json_field(event_id: int, field: str) -> str | None:
    """Get a JSON field from an event by ID. Returns None if missing or empty."""
    from .db import get_db

    conn = get_db()
    row = conn.execute(
        f"SELECT json_extract(data, '$.{field}') as val FROM events WHERE id = ?",
        (event_id,),
    ).fetchone()
    return row["val"] if row and row["val"] else None


def get_thread_from_event(event_id: int) -> str | None:
    """Get thread field from an event by ID."""
    return _get_event_json_field(event_id, "thread")


def get_intent_from_event(event_id: int) -> str | None:
    """Get intent field from an event by ID."""
    return _get_event_json_field(event_id, "intent")


def unescape_bash(text: str) -> str:
    """Remove bash escape sequences from message content.

    Bash escapes special characters when constructing commands. Since hcom
    receives messages as command arguments, we unescape common sequences
    that don't affect the actual message intent.

    NOTE: We do NOT unescape '\\\\' to '\\'. If double backslashes survived
    bash processing, the user intended them (e.g., Windows paths, regex, JSON).
    Unescaping would corrupt legitimate data.
    """
    # Common bash escapes that appear in double-quoted strings
    replacements = [
        ("\\!", "!"),  # History expansion
        ("\\$", "$"),  # Variable expansion
        ("\\`", "`"),  # Command substitution
        ('\\"', '"'),  # Double quote
        ("\\'", "'"),  # Single quote (less common in double quotes but possible)
    ]
    for escaped, unescaped in replacements:
        text = text.replace(escaped, unescaped)
    return text


def send_message(
    identity: SenderIdentity,
    message: str,
    envelope: MessageEnvelope | None = None,
    explicit_targets: list[str] | None = None,
) -> list[str]:
    """Send a message to the database and notify all instances.

    Args:
        identity: Sender identity (kind + name + instance_data)
        message: Message text
        envelope: Optional envelope fields {intent, reply_to, thread}
        explicit_targets: Optional list of explicit target names (from CLI @args).
                         If provided, these are used for routing instead of parsing
                         @mentions from message text.

    Returns:
        delivered_to list (base names of enabled instances that will receive)

    Raises:
        HcomError: If validation fails or database write fails
    """
    # Validate message content (size, control chars, empty)
    validation_error = validate_message(message)
    if validation_error:
        raise HcomError(validation_error)

    from .db import log_event, get_db

    conn = get_db()

    # Get participating instances (row exists = participating)
    # Message delivery works for all instances; session binding gates hook injection
    participating_rows = conn.execute("SELECT name, tag FROM instances").fetchall()
    participating_instances = [{"name": row["name"], "tag": row["tag"]} for row in participating_rows]

    # For target validation: participating instances + CLI identity (bigboss as plain string)
    mentionable = participating_instances + [SENDER]

    # Compute scope and routing data (validates targets against full names)
    scope_result, error = compute_scope(message, mentionable, explicit_targets)
    if error:
        raise HcomError(error)
    assert scope_result is not None  # Guaranteed by compute_scope on success

    scope, extra = scope_result

    # Compute delivered_to: base names of participating instances in scope
    # Build scope dict for should_deliver_message (cross-device stripping is a no-op for local names)
    scope_data: dict[str, Any] = {"scope": scope, **extra}
    delivered_to = [
        inst["name"] for inst in participating_instances
        if should_deliver_message(scope_data, inst["name"], identity.name)
    ]

    # Build event data
    # Note: 'from' and 'delivered_to' store BASE names for DB consistency.
    # Display code converts to full names via get_full_name() at render time.
    data: dict[str, Any] = {
        "from": identity.name,  # Base name (display code converts to full)
        "sender_kind": identity.kind,  # 'external' or 'instance' for filtering
        "scope": scope,  # Routing scope
        "text": message,
        "delivered_to": delivered_to,  # Base names of recipients
    }

    # Add scope extra data (mentions, group_id)
    if extra:
        data.update(extra)

    # Add envelope fields if provided
    if envelope:
        if intent := envelope.get("intent"):
            data["intent"] = intent
        if reply_to := envelope.get("reply_to"):
            data["reply_to"] = reply_to
            # Resolve to local event ID for easier queries
            local_id, _ = resolve_reply_to(reply_to)
            if local_id:
                data["reply_to_local"] = local_id

                # Ack-on-ack loop prevention: don't allow replying to an 'ack' with another 'ack'
                # Ack-on-inform prevention: informational messages don't need acknowledgment
                if intent == "ack":
                    parent_intent = get_intent_from_event(local_id)
                    if parent_intent == "ack":
                        raise HcomError("Ack-on-ack loop detected. Message blocked.")
                    if parent_intent == "inform":
                        raise HcomError("Cannot ack an inform - informational messages don't need acknowledgment.")

        if thread := envelope.get("thread"):
            data["thread"] = thread
        if bundle_id := envelope.get("bundle_id"):
            data["bundle_id"] = bundle_id

    # Log to SQLite with namespace separation
    # External senders use 'ext_{name}' prefix for clear namespace isolation
    # System senders use 'sys_{name}' prefix (e.g., sys_[hcom-launcher])
    # Instance senders use real instance name
    # Actual sender name preserved in data['from'] for display
    if identity.kind == "external":
        routing_instance = f"ext_{identity.name}"
    elif identity.kind == "system":
        routing_instance = f"sys_{identity.name}"
    else:
        routing_instance = identity.name

    try:
        event_id = log_event(event_type="message", instance=routing_instance, data=data)
    except Exception as e:
        raise HcomError(f"Failed to write message to database: {e}")

    # Auto-create request-watch subscriptions for targeted request messages
    if (
        envelope
        and envelope.get("intent") == "request"
        and identity.kind == "instance"
        and scope == "mentions"
    ):
        _create_request_watches(identity.name, event_id, delivered_to)

    # Push to relay server (notify daemon if running, else inline push)
    try:
        from ..relay import trigger_push

        trigger_push()
    except Exception:
        pass  # Best effort

    # Notify all instances after successful write
    from .runtime import notify_all_instances

    notify_all_instances()

    return delivered_to


def send_system_message(sender_name: str, message: str) -> list[str]:
    """Send a system notification message.

    Args:
        sender_name: System sender identifier (e.g., 'hcom-launcher', 'hcom-watchdog')
        message: Message text (can include @mentions for targeting)

    Returns:
        delivered_to list

    Raises:
        HcomError: If validation fails or database write fails
    """
    identity = SenderIdentity(kind="system", name=sender_name, instance_data=None)
    return send_message(identity, message)


def _create_request_watches(sender: str, request_event_id: int, recipients: list[str]) -> None:
    """Create request-watch subscriptions for each recipient.

    Watches for recipients going idle (listening) or stopping without responding.
    Auto-removes on first match (once=True). Cancelled by _cancel_request_watches_by_flow()
    in db.py when the recipient sends a targeted message back to the sender.
    """
    import json
    import time

    from .db import kv_set, get_last_event_id

    last_id = get_last_event_id()
    now = time.time()

    for recipient in recipients:
        sub_id = f"reqwatch-{request_event_id}-{recipient}"
        sub_key = f"events_sub:{sub_id}"

        sql = (
            "(type='status' AND instance=? AND status_val='listening') "
            "OR (type='life' AND instance=? AND life_action='stopped')"
        )

        kv_set(
            sub_key,
            json.dumps(
                {
                    "id": sub_id,
                    "caller": sender,
                    "sql": sql,
                    "params": [recipient, recipient],
                    "filters": {
                        "request_watch": True,
                        "request_id": request_event_id,
                        "target": recipient,
                    },
                    "created": now,
                    "last_id": last_id,
                    "once": True,
                }
            ),
        )


def get_unread_messages(instance_name: str, update_position: bool = False) -> tuple[list[dict[str, Any]], int]:
    """Get unread messages for instance with scope-based filtering.

    Args:
        instance_name: Name of instance to get messages for.
        update_position: If True, mark messages as read by updating position.

    Returns:
        Tuple of (messages, max_event_id) where messages is a list of dicts with keys:
            timestamp (str): ISO timestamp when message was sent.
            from (str): Sender's instance name.
            message (str): Message text content.
            delivered_to (list[str]): List of instance names message was delivered to.
            event_id (int): Database event ID.
            intent (str, optional): Message intent ('request', 'inform', 'ack').
            thread (str, optional): Thread name for grouping related messages.
            _relay (dict, optional): Relay metadata for cross-device messages.
    """
    from .db import get_events_since

    # Get last processed event ID from instance file
    instance_data = load_instance_position(instance_name)
    last_event_id = instance_data.get("last_event_id", 0)

    # Query new message events
    events = get_events_since(last_event_id, event_type="message")

    if not events:
        return [], last_event_id

    messages = []
    for event in events:
        event_data = event["data"]

        if "scope" not in event_data:
            _warn_missing_scope(event["id"])
            continue

        # Skip own messages
        sender_name = event_data["from"]
        if sender_name == instance_name:
            continue

        # Apply scope-based filtering
        try:
            if should_deliver_message(event_data, instance_name, sender_name):
                msg = {
                    "timestamp": event["timestamp"],
                    "from": sender_name,
                    "message": event_data["text"],
                    "delivered_to": event_data.get("delivered_to", []),
                    "event_id": event["id"],
                }
                # Include envelope fields if present
                if intent := event_data.get("intent"):
                    msg["intent"] = intent
                if thread := event_data.get("thread"):
                    msg["thread"] = thread
                if bundle_id := event_data.get("bundle_id"):
                    msg["bundle_id"] = bundle_id
                if relay := event_data.get("_relay"):
                    msg["_relay"] = relay
                messages.append(msg)
        except ValueError as e:
            _warn_corrupt_message(event["id"], e)
            continue

    # Max event ID from events we processed
    max_event_id = events[-1]["id"] if events else last_event_id

    # Only update position (ie mark as read) if explicitly requested (after successful delivery)
    if update_position:
        update_instance_position(instance_name, {"last_event_id": max_event_id})

    return messages, max_event_id


# ==================== Message Filtering & Routing ====================


def should_deliver_message(event_data: dict[str, Any], receiver_name: str, sender_name: str) -> bool:
    """Check if message should be delivered based on scope.

    Args:
        event_data: Message event data with 'scope' field
        receiver_name: Instance to check delivery for
        sender_name: Sender name (excluded from delivery)

    Returns:
        True if receiver should get the message

    Raises:
        KeyError: If scope field missing (old message format)
        ValueError: If scope value invalid
    """
    if receiver_name == sender_name:
        return False

    if "scope" not in event_data:
        raise KeyError("Message missing 'scope' field (old format)")

    scope = event_data["scope"]
    validate_scope(scope)

    if scope == "broadcast":
        return True

    if scope == "mentions":
        mentions = event_data.get("mentions", [])
        # Strip device suffix for cross-device matching
        # e.g., 'mude' matches 'mude:BOXE' after stripping
        receiver_base = receiver_name.split(":")[0]
        return any(receiver_base == m.split(":")[0] for m in mentions)

    return False


def get_subagent_messages(
    parent_name: str, since_id: int = 0, limit: int = 0
) -> tuple[list[dict[str, Any]], int, dict[str, int]]:
    """Get messages from/to subagents of parent instance with scope-based filtering
    Args:
        parent_name: Parent instance name (e.g., 'luna')
        since_id: Event ID to read from (default 0 = all messages)
        limit: Max messages to return (0 = all)
    Returns:
        Tuple of (messages from/to subagents, last_event_id, per_subagent_counts)
        per_subagent_counts: {'luna_reviewer': 2, 'luna_debugger': 0, ...}
    """
    from .db import get_events_since

    # Query all message events since last check
    events = get_events_since(since_id, event_type="message")

    if not events:
        return [], since_id, {}

    # Get all subagent names for this parent using SQL query
    from .db import get_db

    conn = get_db()
    subagent_names = [
        row["name"]
        for row in conn.execute("SELECT name FROM instances WHERE parent_name = ?", (parent_name,)).fetchall()
    ]

    # Initialize per-subagent counts
    per_subagent_counts = {name: 0 for name in subagent_names}
    subagent_names_set = set(subagent_names)  # For fast lookup

    # Filter for messages from/to subagents and track per-subagent counts
    subagent_messages = []
    for event in events:
        event_data = event["data"]

        if "scope" not in event_data:
            _warn_missing_scope(event["id"])
            continue

        sender_name = event_data["from"]

        # Build message dict
        msg = {
            "timestamp": event["timestamp"],
            "from": sender_name,
            "message": event_data["text"],
        }

        # Messages FROM subagents
        if sender_name in subagent_names_set:
            subagent_messages.append(msg)
            # Track which subagents would receive this message
            for subagent_name in subagent_names:
                if subagent_name != sender_name:
                    try:
                        if should_deliver_message(event_data, subagent_name, sender_name):
                            per_subagent_counts[subagent_name] += 1
                    except ValueError as e:
                        _warn_corrupt_message(event["id"], e)
                        continue
        # Messages TO subagents via @mentions or broadcasts
        elif subagent_names:
            # Check which subagents should receive this message
            matched = False
            for subagent_name in subagent_names:
                try:
                    if should_deliver_message(event_data, subagent_name, sender_name):
                        if not matched:
                            subagent_messages.append(msg)
                            matched = True
                        per_subagent_counts[subagent_name] += 1
                except ValueError as e:
                    _warn_corrupt_message(event["id"], e)
                    break  # Skip remaining subagents for this message

    if limit > 0:
        subagent_messages = subagent_messages[-limit:]

    last_event_id = events[-1]["id"] if events else since_id
    return subagent_messages, last_event_id, per_subagent_counts


# ==================== Message Formatting ====================


def _build_message_prefix(msg: dict[str, Any]) -> str:
    """Build message prefix from envelope fields.

    Format: [intent:thread #id] or [intent #id] or [thread:name #id] or [new message #id]
    Remote messages: #id:DEVICE

    Args:
        msg: Message dict with optional 'intent', 'thread', 'event_id', '_relay'

    Returns:
        Formatted prefix string like "[request:pr-42 #42]"
    """
    intent = msg.get("intent")
    thread = msg.get("thread")
    event_id = msg.get("event_id")
    relay = msg.get("_relay", {})

    # Build ID reference (local or remote)
    if relay and relay.get("short") and relay.get("id"):
        id_ref = f"#{relay['id']}:{relay['short']}"
    elif event_id:
        id_ref = f"#{event_id}"
    else:
        id_ref = ""

    # Build prefix based on envelope fields
    if intent and thread:
        prefix = f"{intent}:{thread}"
    elif intent:
        prefix = intent
    elif thread:
        prefix = f"thread:{thread}"
    else:
        prefix = "new message"

    if id_ref:
        return f"[{prefix} {id_ref}]"
    return f"[{prefix}]"


def format_hook_messages(messages: list[dict[str, Any]], instance_name: str) -> str:
    """Format messages for hook feedback.

    Single message uses verbose format: "sender → recipient + N others"
    Multiple messages use compact format: "sender → recipient (+N)"

    Format includes envelope info: [intent:thread #id] sender → recipient: text
    """
    from .instances import get_full_name, get_display_name

    def _others_count(msg: dict[str, Any]) -> int:
        """Count other recipients (excluding self)"""
        delivered_to = msg.get("delivered_to", [])
        # Others = total recipients minus self
        return max(0, len(delivered_to) - 1)

    def _get_sender_display_name(sender_base_name: str) -> str:
        """Get full display name for sender (base name -> tag-base or base)"""
        sender_data = load_instance_position(sender_base_name)
        return get_full_name(sender_data) or sender_base_name

    recipient_display = get_display_name(instance_name)

    if len(messages) == 1:
        msg = messages[0]
        others = _others_count(msg)
        if others > 0:
            recipient = f"{recipient_display} (+{others} other{'s' if others > 1 else ''})"
        else:
            recipient = recipient_display
        prefix = _build_message_prefix(msg)
        sender_display = _get_sender_display_name(msg["from"])
        reason = f"{prefix} {sender_display} → {recipient}: {msg['message']}"
    else:
        parts = []
        for msg in messages:
            others = _others_count(msg)
            if others > 0:
                recipient = f"{recipient_display} (+{others})"
            else:
                recipient = recipient_display
            prefix = _build_message_prefix(msg)
            sender_display = _get_sender_display_name(msg["from"])
            parts.append(f"{prefix} {sender_display} → {recipient}: {msg['message']}")
        reason = f"[{len(messages)} new messages] | {' | '.join(parts)}"

    # Append hints to messages: instance-specific first, then global config
    instance_data = load_instance_position(instance_name)
    hints = None
    if instance_data:
        hints = instance_data.get("hints")  # Per-instance override
    if not hints:
        hints = get_config().hints  # Global fallback
    if hints:
        reason = f"{reason} | [{hints}]"

    # Show recv:intent tip on first receipt of each intent type
    from .tips import has_seen_tip, mark_tip_seen, TIPS

    for msg in messages:
        intent = msg.get("intent")
        if intent:
            tip_key = f"recv:intent:{intent}"
            if tip_key in TIPS and not has_seen_tip(instance_name, tip_key):
                mark_tip_seen(instance_name, tip_key)
                reason = f"{reason}\n{TIPS[tip_key]}"
                break  # Only show one tip per delivery

    return reason


def format_messages_json(messages: list[dict[str, Any]], instance_name: str) -> str:
    """Format messages for model injection.

    Wraps format_hook_messages output in <hcom> tags for model context.
    Uses the same human-readable format for both terminal display and model injection.

    Output: <hcom>[request #42] sender → recipient: hello world</hcom>
    """
    return f"<hcom>{format_hook_messages(messages, instance_name)}</hcom>"


def get_read_receipts(
    identity: SenderIdentity, max_text_length: int = 50, limit: int | None = None
) -> list[ReadReceipt]:
    """Get read receipts for messages sent by sender.
    Args:
        identity: SenderIdentity for the sender (external or instance)
        max_text_length: Maximum text length before truncation (default 50)
        limit: Maximum number of recent messages to return (default None = all)
    Returns:
        List of dicts with keys: id, age, text, read_by, total_recipients
    """
    from .db import get_db
    from datetime import datetime, timezone
    import json

    conn = get_db()

    # Determine storage name: external senders use ext_ prefix, instances use real name
    storage_name = f"ext_{identity.name}" if identity.kind == "external" else identity.name

    # Query by storage name
    query = """
        SELECT e.id, e.timestamp, e.data
        FROM events e
        WHERE e.type = 'message'
          AND e.instance = ?
        ORDER BY e.id DESC
    """

    if limit is not None:
        query += f" LIMIT {int(limit)}"

    sent_messages = conn.execute(query, (storage_name,)).fetchall()

    if not sent_messages:
        return []

    # Get instance metadata for remote detection and external sender checks
    # Note: We use events for read status (survives instance deletion), but still
    # need instance data for origin_device_id (remote) and external sender checks
    active_instances_query = """
        SELECT name, tag, session_id, parent_session_id, origin_device_id
        FROM instances
        WHERE name != ?
    """
    active_instances = conn.execute(active_instances_query, (identity.name,)).fetchall()
    instance_data_cache = {
        row["name"]: {
            "tag": row["tag"],
            "session_id": row["session_id"],
            "parent_session_id": row["parent_session_id"],
            "origin_device_id": row["origin_device_id"],
        }
        for row in active_instances
    }

    # For remote instances, get their max msg_ts from status events
    remote_msg_ts = {}
    remote_instances = [row["name"] for row in active_instances if row["origin_device_id"]]
    if remote_instances:
        for inst_name in remote_instances:
            row = conn.execute(
                """
                SELECT json_extract(data, '$.msg_ts') as msg_ts
                FROM events
                WHERE type = 'status' AND instance = ?
                  AND json_extract(data, '$.msg_ts') IS NOT NULL
                ORDER BY id DESC LIMIT 1
            """,
                (inst_name,),
            ).fetchone()
            if row and row["msg_ts"]:
                remote_msg_ts[inst_name] = row["msg_ts"]

    receipts = []
    now = datetime.now(timezone.utc)

    for msg_row in sent_messages:
        msg_id = msg_row["id"]
        msg_timestamp = msg_row["timestamp"]
        msg_data = json.loads(msg_row["data"])
        msg_text = msg_data["text"]

        # Validate scope field present (skip old messages)
        if "scope" not in msg_data:
            continue

        # Use delivered_to for read receipt denominator
        if "delivered_to" not in msg_data:
            continue

        delivered_to = msg_data["delivered_to"]

        # Find recipients that HAVE read this message using events (survives instance deletion)
        # Query for status:deliver:* events after this message from any recipient
        deliver_events = conn.execute(
            """
            SELECT DISTINCT instance
            FROM events
            WHERE type = 'status'
              AND id > ?
              AND json_extract(data, '$.context') LIKE 'deliver:%'
              AND instance IN (SELECT value FROM json_each(?))
        """,
            (msg_id, json.dumps(delivered_to)),
        ).fetchall()
        delivered_instances = {row["instance"] for row in deliver_events}

        read_by = []
        for inst_name in delivered_to:
            inst_data = instance_data_cache.get(inst_name)

            # Remote instance: compare msg_ts (timestamp-based)
            if inst_data and inst_data.get("origin_device_id"):
                if inst_name in remote_msg_ts and remote_msg_ts[inst_name] >= msg_timestamp:
                    read_by.append(inst_name)
                continue

            # Local instance: check for deliver event after message (survives instance deletion)
            if inst_name in delivered_instances:
                # For external senders, only mark as read if they were @mentioned
                if inst_data:
                    from ..core.instances import is_external_sender

                    if is_external_sender(inst_data):
                        inst_tag = inst_data.get("tag")
                        if not is_mentioned(msg_text, inst_name, inst_tag):
                            continue
                read_by.append(inst_name)

        total_recipients = len(delivered_to)

        if total_recipients > 0:
            # Calculate age
            msg_time = parse_iso_timestamp(msg_timestamp)
            age_str = format_age((now - msg_time).total_seconds()) if msg_time else "?"

            # Truncate text
            if len(msg_text) > max_text_length:
                truncated_text = msg_text[: max_text_length - 3] + "..."
            else:
                truncated_text = msg_text

            receipts.append(
                {
                    "id": msg_id,
                    "age": age_str,
                    "text": truncated_text,
                    "read_by": read_by,
                    "total_recipients": total_recipients,
                }
            )

    return receipts  # type: ignore[return-value]


def get_unread_counts_batch(instances: dict[str, dict[str, Any]]) -> dict[str, int]:
    """Get unread message count per instance efficiently.

    Uses a single DB query to fetch all messages since minimum waterline,
    then counts per instance with scope filtering.

    Args:
        instances: {name: instance_data} with last_event_id per instance

    Returns:
        {name: unread_count}
    """
    from .db import get_events_since

    if not instances:
        return {}

    # Extract last_event_id per instance
    positions: dict[str, int] = {}
    for name, data in instances.items():
        positions[name] = data.get("last_event_id", 0)

    # Find minimum waterline (oldest unread position)
    min_waterline = min(positions.values()) if positions else 0

    # Single query for all messages since minimum waterline
    events = get_events_since(min_waterline, event_type="message")

    if not events:
        return {name: 0 for name in instances}

    # Count per instance with scope filtering
    counts = {name: 0 for name in instances}
    for event in events:
        event_id = event["id"]
        event_data = event["data"]

        # Validate scope field present
        if "scope" not in event_data:
            continue

        sender_name = event_data.get("from", "")

        # For each instance, check if they should receive this message
        for instance_name, last_id in positions.items():
            # Skip if instance already processed this event
            if event_id <= last_id:
                continue

            # Skip own messages
            if instance_name == sender_name:
                continue

            # Apply scope-based filtering
            try:
                if should_deliver_message(event_data, instance_name, sender_name):
                    counts[instance_name] += 1
            except (KeyError, ValueError):
                continue

    return counts


# ==================== PTY Message Preview ====================
# Moved from pty/pty_common.py to centralize message formatting

# Max length for message preview in PTY trigger
PREVIEW_MAX_LEN = 60


def build_message_preview(instance_name: str, max_len: int = PREVIEW_MAX_LEN) -> str:
    """Build truncated message preview for PTY injection.

    Used by Gemini and Claude PTY modes to show a preview hint in the
    injected trigger while avoiding problematic characters.

    Reuses format_hook_messages but truncates before user message content.
    User content may contain @ chars that trigger autocomplete in some CLIs.
    Full messages delivered via hook's additionalContext.
    """
    messages, _ = get_unread_messages(instance_name, update_position=False)
    if not messages:
        return "<hcom></hcom>"

    wrapper_open = "<hcom>"
    wrapper_close = "</hcom>"
    wrapper_len = len(wrapper_open) + len(wrapper_close)
    content_max = max_len - wrapper_len
    if content_max <= 0:
        return f"{wrapper_open}{wrapper_close}"

    formatted = format_hook_messages(messages, instance_name)

    # Truncate before user content (after first ": ") to avoid special chars
    colon_pos = formatted.find(": ")
    if colon_pos != -1:
        envelope = formatted[:colon_pos]  # Stop before the colon
        if len(envelope) > content_max:
            if content_max <= 3:
                return f"{wrapper_open}{wrapper_close}"
            return f"{wrapper_open}{envelope[: content_max - 3]}...{wrapper_close}"
        return f"{wrapper_open}{envelope}{wrapper_close}"

    # No colon found, just truncate normally
    if len(formatted) > content_max:
        if content_max <= 3:
            return f"{wrapper_open}{wrapper_close}"
        return f"{wrapper_open}{formatted[: content_max - 3]}...{wrapper_close}"
    return f"{wrapper_open}{formatted}{wrapper_close}"


__all__ = [
    # Type definitions
    "MessageScope",
    "MessageIntent",
    "MessageEnvelope",
    "RelayMetadata",
    "MessageEventData",
    "UnreadMessage",
    "FormattedMessage",
    "ReadReceipt",
    "ScopeExtra",
    "ScopeResult",
    # Functions
    "format_recipients",
    "compute_scope",
    "unescape_bash",
    "send_message",
    "get_unread_messages",
    "should_deliver_message",
    "get_subagent_messages",
    "format_hook_messages",
    "format_messages_json",
    "get_read_receipts",
    "get_unread_counts_batch",
    # PTY helpers
    "build_message_preview",
    "PREVIEW_MAX_LEN",
]
