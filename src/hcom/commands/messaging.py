"""Messaging commands for HCOM"""

import socket
import sys
import time

from .utils import (
    CLIError,
    format_error,
    parse_flag_bool,
    parse_flag_value,
    resolve_identity,
    validate_flags,
    validate_message,
)
from ..shared import MAX_MESSAGES_PER_DELIVERY, SENDER, HcomError, CommandContext
from ..core.paths import ensure_hcom_directories
from ..core.db import init_db
from ..core.instances import load_instance_position, set_status, get_instance_status
from ..core.messages import (
    unescape_bash,
    send_message,
    get_unread_messages,
    format_hook_messages,
    format_messages_json,
)


# ==================== Shared Helpers ====================

# Import centralized TCP server creation
from ..core.runtime import create_notify_server as _create_notify_server


def _init_heartbeat(instance_name: str, timeout: float) -> None:
    """Initialize heartbeat fields for listen loop."""
    from ..core.instances import update_instance_position

    try:
        update_instance_position(
            instance_name,
            {
                "last_stop": int(time.time()),
                "wait_timeout": timeout,
            },
        )
    except Exception:
        pass


def _update_heartbeat(instance_name: str) -> None:
    """Update heartbeat timestamp during listen loop."""
    from ..core.instances import update_instance_position

    try:
        update_instance_position(instance_name, {"last_stop": int(time.time())})
    except Exception:
        pass


def _drain_notify_server(server: socket.socket) -> None:
    """Accept and close all pending connections on notify server."""
    while True:
        try:
            server.accept()[0].close()
        except BlockingIOError:
            break


# ==================== Recipient Feedback ====================


def get_recipient_feedback(delivered_to: list[str]) -> str:
    """Get formatted recipient feedback showing who received the message.

    Args:
        delivered_to: Instances that received the message (base names from send_message)

    Returns:
        Formatted string like "Sent to: ◉ luna, ◉ nova" (with full display names)
    """
    from ..shared import STATUS_ICONS, ADHOC_ICON
    from ..core.instances import get_full_name

    if not delivered_to:
        # No agents will receive, but bigboss (human at TUI) can see all messages
        return f"Sent to: {SENDER} (no other active agents)"

    # Format recipients with status icons
    if len(delivered_to) > 10:
        return f"Sent to {len(delivered_to)} agents"

    recipient_status = []
    for r_name in delivered_to:
        r_data = load_instance_position(r_name)
        if r_data:
            status, _, _, _, _ = get_instance_status(r_data)
            icon = STATUS_ICONS.get(status, STATUS_ICONS["inactive"])
            display_name = get_full_name(r_data) or r_name
        else:
            icon = ADHOC_ICON  # Unknown recipient - neutral (not claiming dead)
            display_name = r_name
        recipient_status.append(f"{icon} {display_name}")

    return f"Sent to: {', '.join(recipient_status)}"


def cmd_send(
    argv: list[str], quiet: bool = False, *, ctx: CommandContext | None = None
) -> int:
    """Send message to hcom: hcom send "message" [--name NAME] [--from NAME]"""
    if not ensure_hcom_directories():
        print(format_error("Failed to create HCOM directories"), file=sys.stderr)
        return 1

    init_db()

    # Validate: reject unknown flags (common hallucination: -t, -m, -a, etc.)
    if error := validate_flags("send", argv):
        print(format_error(error), file=sys.stderr)
        return 1

    # Parse --from flag (external sender identity)
    # -b is alias for --from bigboss (human override)
    from ..shared import SenderIdentity

    # Handle -b alias for --from bigboss (parse_flag_bool copies argv internally)
    has_bigboss, argv = parse_flag_bool(argv, "-b")
    from_name: str | None = "bigboss" if has_bigboss else None

    # Parse --from (overrides -b if both present)
    try:
        from_val, argv = parse_flag_value(argv, "--from")
        if from_val:
            from_name = from_val
    except CLIError as e:
        print(format_error(str(e)), file=sys.stderr)
        return 1

    # Validate --from name if provided
    if from_name:
        from ..core.identity import validate_name_input

        if error := validate_name_input(from_name, allow_at=False):
            print(format_error(error), file=sys.stderr)
            return 1

    # Guard: subagents must not spoof external sender identities.
    if from_name:
        actor = ctx.identity if (ctx and ctx.identity) else None
        if actor is None:
            try:
                actor = resolve_identity()
            except Exception:
                actor = None
        if (
            actor
            and actor.kind == "instance"
            and actor.instance_data
            and actor.instance_data.get("parent_name")
        ):
            print(
                format_error(
                    "Subagents cannot use --from/-b (external sender spoofing)"
                ),
                file=sys.stderr,
            )
            return 1

    # Identity (instance-only): CLI supplies ctx (preferred). Direct calls may still pass --name.
    explicit_name = ctx.explicit_name if ctx else None
    if ctx is None:
        from .utils import parse_name_flag
        from ..core.identity import (
            _looks_like_uuid,
            is_valid_base_name,
            base_name_error,
            validate_name_input,
        )

        explicit_name, argv = parse_name_flag(argv)

        # If --name provided, validate (instance identity)
        if explicit_name:
            # Skip validation for UUIDs (agent_id format)
            if not _looks_like_uuid(explicit_name):
                # Validate length and dangerous characters
                if error := validate_name_input(explicit_name, allow_at=True):
                    print(format_error(error), file=sys.stderr)
                    return 1
                if not is_valid_base_name(explicit_name):
                    print(format_error(base_name_error(explicit_name)), file=sys.stderr)
                    return 1

    # Extract envelope flags (optional structured messaging)
    envelope = {}

    # --intent {request|inform|ack|error}
    try:
        intent_val, argv = parse_flag_value(argv, "--intent")
        if intent_val:
            from ..core.helpers import validate_intent

            intent_val = intent_val.lower()
            try:
                validate_intent(intent_val)
            except ValueError as e:
                print(format_error(str(e)), file=sys.stderr)
                return 1
            envelope["intent"] = intent_val
    except CLIError:
        print(format_error("--intent requires a value (request|inform|ack|error)"), file=sys.stderr)
        return 1

    # --reply-to <id> or <id:DEVICE>
    try:
        reply_to_val, argv = parse_flag_value(argv, "--reply-to")
        if reply_to_val:
            envelope["reply_to"] = reply_to_val
    except CLIError:
        print(format_error("--reply-to requires an event ID (e.g., 42 or 42:BOXE)"), file=sys.stderr)
        return 1

    # --thread <name>
    try:
        thread_val, argv = parse_flag_value(argv, "--thread")
        if thread_val:
            # Validate thread name
            if len(thread_val) > 64:
                print(format_error(f"Thread name too long ({len(thread_val)} chars, max 64)"), file=sys.stderr)
                return 1
            if not all(c.isalnum() or c in "-_" for c in thread_val):
                print(format_error("Thread name must be alphanumeric with hyphens/underscores"), file=sys.stderr)
                return 1
            envelope["thread"] = thread_val
    except CLIError:
        print(format_error("--thread requires a thread name"), file=sys.stderr)
        return 1

    # Optional: bundle payload (JSON or file)
    import json as json_mod
    from pathlib import Path

    bundle_data = None

    try:
        bundle_raw, argv = parse_flag_value(argv, "--bundle")
        if bundle_raw:
            try:
                bundle_data = json_mod.loads(bundle_raw)
            except json_mod.JSONDecodeError as e:
                print(format_error(f"Invalid --bundle JSON: {e}"), file=sys.stderr)
                return 1
    except CLIError:
        print(format_error("--bundle requires JSON value"), file=sys.stderr)
        return 1

    try:
        bundle_file, argv = parse_flag_value(argv, "--bundle-file")
        if bundle_file:
            if bundle_data is not None:
                print(format_error("--bundle and --bundle-file are mutually exclusive"), file=sys.stderr)
                return 1
            path = Path(bundle_file)
            try:
                raw = path.read_text(encoding="utf-8")
                bundle_data = json_mod.loads(raw)
            except OSError as e:
                print(format_error(f"Failed to read --bundle-file: {e}"), file=sys.stderr)
                return 1
            except json_mod.JSONDecodeError as e:
                print(format_error(f"Invalid --bundle-file JSON: {e}"), file=sys.stderr)
                return 1
    except CLIError:
        print(format_error("--bundle-file requires a path"), file=sys.stderr)
        return 1

    # Validation: ack requires reply_to
    if envelope.get("intent") == "ack" and "reply_to" not in envelope:
        print(format_error("Intent 'ack' requires --reply-to"), file=sys.stderr)
        return 1

    # Validate reply_to exists and inherit thread if not explicit
    if "reply_to" in envelope:
        from ..core.messages import resolve_reply_to, get_thread_from_event

        local_id, error = resolve_reply_to(envelope["reply_to"])
        if error:
            print(format_error(f"Invalid --reply-to: {error}"), file=sys.stderr)
            return 1
        # Thread inheritance: if reply_to without explicit thread, inherit from parent
        if "thread" not in envelope and local_id:
            parent_thread = get_thread_from_event(local_id)
            if parent_thread:
                envelope["thread"] = parent_thread

    # Resolve message from args or stdin
    use_stdin, argv = parse_flag_bool(argv, "--stdin")

    def _read_stdin() -> str:
        try:
            return sys.stdin.read()
        except OSError:
            return ""

    message: str | None = None
    if use_stdin:
        if argv:
            print(
                format_error("--stdin cannot be combined with message arguments"),
                file=sys.stderr,
            )
            return 1
        message = _read_stdin()
        if not message:
            print(format_error("No input received on stdin"), file=sys.stderr)
            return 1
    elif not argv and not sys.stdin.isatty():
        message = _read_stdin()
        if not message:
            print(format_error("No input received on stdin"), file=sys.stderr)
            return 1
    elif len(argv) > 1:
        print(
            format_error("Message must be a single argument or piped via stdin"),
            file=sys.stderr,
        )
        return 1
    else:
        message = unescape_bash(argv[0]) if argv else None

    # Check message provided
    if not message:
        from .utils import get_command_help

        print(format_error("No message provided") + "\n", file=sys.stderr)
        print(get_command_help("send"), file=sys.stderr)
        return 1

    # Only validate and send if message is provided
    identity: SenderIdentity | None = None
    if message:
        # Validate message
        error = validate_message(message)
        if error:
            print(error, file=sys.stderr)
            return 1

        # Resolve sender identity
        # - --from: one-shot external sender
        # - --name: strict instance lookup
        # - Neither: auto-detect from environment
        if from_name:
            identity = SenderIdentity(kind="external", name=from_name)
        elif ctx and ctx.identity:
            identity = ctx.identity
        elif explicit_name:
            identity = resolve_identity(name=explicit_name)
        else:
            identity = resolve_identity()

        # Guard: Block sends from vanilla Claude before opt-in
        import os

        if (
            identity.kind == "instance"
            and not identity.instance_data
            and os.environ.get("CLAUDECODE") == "1"
        ):
            print(format_error("Cannot send without identity."), file=sys.stderr)
            print("Run 'hcom start' first, then use 'hcom send'.", file=sys.stderr)
            return 1

        # For instances (not external), row existence = participating
        # (No enabled check needed - row exists means active)

        # Status set by _set_hookless_command_status in cli.py for subagent/codex/adhoc

        # Pull remote state to ensure delivered_to includes cross-device instances
        try:
            from ..relay import is_relay_handled_by_tui, pull

            if not is_relay_handled_by_tui():
                pull()  # relay.py logs errors internally
        except Exception:
            pass  # Best-effort - local send still works

        # Create bundle event if provided
        if bundle_data is not None:
            from ..core.bundles import create_bundle_event, validate_bundle

            try:
                hints = validate_bundle(bundle_data)
            except ValueError as e:
                print(format_error(str(e)), file=sys.stderr)
                return 1
            if hints:
                print("Bundle quality hints:", file=sys.stderr)
                for h in hints:
                    print(f"  - {h}", file=sys.stderr)

            if identity.kind == "external":
                bundle_instance = f"ext_{identity.name}"
            elif identity.kind == "system":
                bundle_instance = f"sys_{identity.name}"
            else:
                bundle_instance = identity.name

            bundle_id = create_bundle_event(
                bundle_data, instance=bundle_instance, created_by=identity.name
            )
            envelope["bundle_id"] = bundle_id
            # Append bundle payload to message text
            refs = bundle_data.get("refs", {})
            events = refs.get("events", [])
            files = refs.get("files", [])
            transcript = refs.get("transcript", [])
            extends = bundle_data.get("extends")

            def _join(vals):
                return ", ".join(str(v) for v in vals) if vals else ""

            bundle_lines = [
                f"[Bundle {bundle_id}]",
                f"Title: {bundle_data.get('title', '')}",
                f"Description: {bundle_data.get('description', '')}",
                "Refs:",
                f"  events: {_join(events)}",
                f"  files: {_join(files)}",
                f"  transcript: {_join(transcript)}",
            ]
            if extends:
                bundle_lines.append(f"Extends: {extends}")

            message = message.rstrip() + "\n\n" + "\n".join(bundle_lines)
            # Re-validate after append to avoid oversized payloads
            error = validate_message(message)
            if error:
                print(error, file=sys.stderr)
                return 1

        # Send message and get delivered_to list
        try:
            delivered_to = send_message(
                identity, message, envelope if envelope else None
            )
        except HcomError as e:
            print(f"Error: {e}", file=sys.stderr)
            return 1

        # Handle quiet mode
        if quiet:
            return 0

        # Get recipient feedback
        recipient_feedback = get_recipient_feedback(delivered_to)

        # Show unread messages if instance context
        if identity.kind == "instance":
            from ..core.db import get_db

            conn = get_db()
            messages, _ = get_unread_messages(identity.name, update_position=True)
            if messages:
                subagent_names = {
                    row["name"]
                    for row in conn.execute(
                        "SELECT name FROM instances WHERE parent_name = ?",
                        (identity.name,),
                    ).fetchall()
                }

                # Separate subagent messages from main messages
                subagent_msgs = []
                main_msgs = []
                for msg in messages:
                    sender = msg["from"]
                    if sender in subagent_names:
                        subagent_msgs.append(msg)
                    else:
                        main_msgs.append(msg)

                output_parts = [recipient_feedback]
                max_msgs = MAX_MESSAGES_PER_DELIVERY

                if main_msgs:
                    formatted = format_hook_messages(
                        main_msgs[:max_msgs], identity.name
                    )
                    output_parts.append(f"\n{formatted}")

                if subagent_msgs:
                    formatted = format_hook_messages(
                        subagent_msgs[:max_msgs], identity.name
                    )
                    output_parts.append(f"\n[Subagent messages]\n{formatted}")

                print("".join(output_parts))
            else:
                print(recipient_feedback)
        else:
            # External sender - just show feedback
            print(recipient_feedback)

    # For adhoc instances (--name with instance), append unread messages
    # This delivers pending messages to external tools using hcom send --name <name>
    if explicit_name and identity is not None and identity.kind == "instance":
        from .utils import append_unread_messages

        append_unread_messages(identity.name)

    return 0


def _listen_with_filter(
    sql_filter: str,
    instance_name: str,
    timeout: float,
    json_output: bool,
    instance_data: dict,
) -> int:
    """Listen mode with SQL filter - uses temp subscription.

    Creates a temp --once subscription, waits for match or timeout.
    Subscription auto-deletes on first match; cleanup on timeout.
    """
    import select
    import json as json_mod
    from hashlib import sha256
    from ..core.db import (
        get_db,
        kv_set,
        get_last_event_id,
        upsert_notify_endpoint,
        delete_notify_endpoint,
    )
    from ..relay import relay_wait, is_relay_enabled

    conn = get_db()

    # Validate SQL syntax
    try:
        conn.execute(f"SELECT 1 FROM events_v WHERE ({sql_filter}) LIMIT 0")
    except Exception as e:
        print(f"Invalid SQL filter: {e}", file=sys.stderr)
        return 1

    # Check for recent match (10s lookback) - return immediately if found
    from datetime import datetime, timezone

    lookback_ts = datetime.fromtimestamp(time.time() - 10, tz=timezone.utc).isoformat()
    recent = conn.execute(
        f"SELECT id, timestamp, type, instance, data FROM events_v WHERE timestamp > ? AND ({sql_filter}) ORDER BY id DESC LIMIT 1",
        [lookback_ts],
    ).fetchone()
    if recent:
        if json_output:
            print(
                json_mod.dumps(
                    {
                        "event_id": recent["id"],
                        "type": recent["type"],
                        "instance": recent["instance"],
                        "data": json_mod.loads(recent["data"])
                        if recent["data"]
                        else {},
                    }
                )
            )
        else:
            print(
                f"[Match found] #{recent['id']} {recent['type']}:{recent['instance']}"
            )
        return 0

    # Create temp --once subscription
    now = time.time()
    sub_id = (
        f"listen-{sha256(f'{instance_name}{sql_filter}{now}'.encode()).hexdigest()[:6]}"
    )
    sub_key = f"events_sub:{sub_id}"

    # Mark instance as listening BEFORE capturing last_id
    # This ensures our own status event is excluded from the subscription
    set_status(instance_name, "listening", f"filter:{sub_id}")

    kv_set(
        sub_key,
        json_mod.dumps(
            {
                "id": sub_id,
                "sql": sql_filter,
                "caller": instance_name,
                "once": True,
                "last_id": get_last_event_id(),
                "created": now,
            }
        ),
    )

    # Setup TCP notify socket
    notify_server, notify_port = _create_notify_server()
    if notify_port:
        upsert_notify_endpoint(instance_name, "listen_filter", notify_port)

    # Initialize heartbeat
    start_time = time.time()
    _init_heartbeat(instance_name, timeout)

    if not json_output:
        print(
            f"[Listening for events matching filter. Timeout: {timeout}s]",
            file=sys.stderr,
        )

    try:
        while (time.time() - start_time) < timeout:
            # Check if instance was stopped externally
            current = load_instance_position(instance_name)
            if not current:
                if not json_output:
                    print(
                        f"\n[Disconnected: HCOM stopped for {instance_name}]",
                        file=sys.stderr,
                    )
                return 0

            # Relay sync (long-poll)
            remaining = timeout - (time.time() - start_time)
            if is_relay_enabled():
                try:
                    relay_wait(min(remaining, 25))
                except Exception:
                    pass

            # Check for subscription message (delivered via send_system_message)
            messages, _ = get_unread_messages(instance_name, update_position=True)
            if messages:
                # Look for subscription notification from [hcom-events]
                for msg in messages:
                    if msg.get(
                        "from"
                    ) == "[hcom-events]" and f"[sub:{sub_id}]" in msg.get(
                        "message", ""
                    ):
                        # Match found - subscription auto-deleted by --once
                        if json_output:
                            # Parse event details from notification
                            print(
                                json_mod.dumps(
                                    {
                                        "matched": True,
                                        "notification": msg.get("message", ""),
                                    }
                                )
                            )
                        else:
                            print(f"\n{msg.get('message', '')}")
                        set_status(instance_name, "active", "filter matched")
                        return 0

                # Other messages received - still waiting for filter match
                # But show them if not json mode
                if not json_output:
                    formatted = format_hook_messages(messages, instance_name)
                    print(f"\n{formatted}")
                    print("[Still waiting for filter match...]", file=sys.stderr)

            # Update heartbeat
            _update_heartbeat(instance_name)

            # TCP select for local notifications
            remaining = timeout - (time.time() - start_time)
            if remaining <= 0:
                break
            wait_time = min(remaining, 1.0 if is_relay_enabled() else 5.0)

            if notify_server:
                readable, _, _ = select.select([notify_server], [], [], wait_time)
                if readable:
                    _drain_notify_server(notify_server)
            else:
                time.sleep(wait_time)

        # Timeout - cleanup subscription
        if not json_output:
            print(f"\n[Timeout: no match after {timeout}s]", file=sys.stderr)
        if instance_data.get("tool") == "adhoc":
            set_status(instance_name, "inactive", "exit:timeout")
        return 0

    except KeyboardInterrupt:
        if not json_output:
            print("\n[Interrupted]", file=sys.stderr)
        return 130
    finally:
        # Cleanup subscription if it still exists (timeout case)
        kv_set(sub_key, None)
        # Cleanup notify endpoint
        try:
            delete_notify_endpoint(instance_name, kind="listen_filter")
        except Exception:
            pass
        if notify_server:
            try:
                notify_server.close()
            except Exception:
                pass


def cmd_listen(argv: list[str], *, ctx: CommandContext | None = None) -> int:
    """Block and receive messages: hcom listen --name NAME [--timeout N] [--json] [--sql FILTER]"""
    import select
    from ..relay import is_relay_enabled

    if not ensure_hcom_directories():
        print(format_error("Failed to create HCOM directories"), file=sys.stderr)
        return 1

    init_db()

    # Validate flags
    if error := validate_flags("listen", argv):
        print(format_error(error), file=sys.stderr)
        return 1

    # Identity: CLI supplies ctx (preferred). Direct calls may still pass --name.
    name_value = ctx.explicit_name if ctx else None
    if ctx is None:
        from .utils import parse_name_flag

        name_value, argv = parse_name_flag(argv)

    if ctx and ctx.identity:
        identity = ctx.identity
        instance_name = identity.name
    elif name_value:
        # Explicit --name provided - strict instance lookup
        try:
            identity = resolve_identity(name=name_value)
        except HcomError as e:
            print(format_error(str(e)), file=sys.stderr)
            return 1
        instance_name = identity.name
    else:
        # No explicit --name - resolve from environment
        try:
            identity = resolve_identity()
            instance_name = identity.name
        except Exception:
            print(
                format_error("--name required (no identity context)"), file=sys.stderr
            )
            print("Usage: hcom listen --name <name> [--timeout N]", file=sys.stderr)
            return 1

    # Parse --timeout (optional, default 24 hours - matches HCOM_TIMEOUT)
    timeout: int | float = 86400  # 24 hours default
    try:
        timeout_val, argv = parse_flag_value(argv, "--timeout")
        if timeout_val:
            try:
                timeout = int(timeout_val)
            except ValueError:
                print(format_error(f"--timeout must be an integer, got '{timeout_val}'"), file=sys.stderr)
                return 1
    except CLIError:
        print(format_error("--timeout requires a value"), file=sys.stderr)
        return 1

    # Parse positional timeout (e.g. hcom listen 5)
    # Only consume if it looks like an integer and we haven't set timeout via flag
    if timeout_val is None:
        for i, arg in enumerate(argv):
            if not arg.startswith("-") and arg.isdigit():
                timeout = int(arg)
                argv = argv[:i] + argv[i + 1 :]
                break

    # Quick check mode: timeout <= 1 means instant check (0.1s)
    if timeout <= 1:
        timeout = 0.1

    # Parse --json (optional)
    json_output, argv = parse_flag_bool(argv, "--json")

    # PHASE 1: Expand filter shortcuts (--idle, --blocked)
    from ..core.filters import expand_shortcuts
    argv = expand_shortcuts(argv)

    # PHASE 2: Parse composable filter flags
    from ..core.filters import parse_event_flags, build_sql_from_flags
    filters, argv = parse_event_flags(argv)

    # PHASE 3: Validate type constraints
    if filters:
        from ..core.filters import validate_type_constraints
        try:
            validate_type_constraints(filters)
        except ValueError as e:
            print(format_error(str(e)), file=sys.stderr)
            return 1

    # Parse --sql (optional) - SQL filter mode
    try:
        sql_filter, argv = parse_flag_value(argv, "--sql")
    except CLIError:
        print(format_error("--sql requires a value"), file=sys.stderr)
        return 1

    # Use instance_data from identity resolution (already validated by resolve_identity)
    instance_data = identity.instance_data
    if not instance_data:
        # Shouldn't happen - resolve_identity raises if instance not found
        print(format_error(f"hcom not started for '{instance_name}'."), file=sys.stderr)
        return 1

    # PHASE 4: Combine filters and --sql (both work together, ANDed)
    combined_sql = None
    if filters or sql_filter:
        sql_parts = []

        # Add filter SQL
        if filters:
            flag_sql = build_sql_from_flags(filters)
            if flag_sql:
                sql_parts.append(f"({flag_sql})")

        # Add manual SQL filter
        if sql_filter:
            sql_parts.append(f"({sql_filter})")

        # Combine with AND
        if sql_parts:
            combined_sql = " AND ".join(sql_parts)

    # Branch: SQL filter mode vs message-wait mode
    if combined_sql:
        return _listen_with_filter(
            combined_sql, instance_name, timeout, json_output, instance_data
        )

    # Standard message-wait mode below
    # Mark instance as listening when entering listen loop
    set_status(instance_name, "listening", "ready")

    start_time = time.time()

    # Setup TCP notification socket for instant wake on local messages
    notify_server, notify_port = _create_notify_server()

    # Initialize heartbeat
    _init_heartbeat(instance_name, timeout)

    # Register notify endpoint without clobbering other listeners (PTY wrappers, hooks)
    if notify_port:
        try:
            from ..core.db import upsert_notify_endpoint

            upsert_notify_endpoint(instance_name, "listen", int(notify_port))
        except Exception:
            pass

    # Check if already disconnected before starting polling (row deleted = stopped)
    current_instance = load_instance_position(instance_name)
    if not current_instance:
        print("[You have been disconnected from HCOM]", file=sys.stderr)
        if notify_server:
            notify_server.close()
        return 0

    if not json_output:
        print(
            f"[Listening for messages to {instance_name}. Timeout: {timeout}s]",
            file=sys.stderr,
        )

    try:
        while (time.time() - start_time) < timeout:
            # Check if instance was stopped externally (row deleted = stopped)
            current_instance = load_instance_position(instance_name)
            if not current_instance:
                if not json_output:
                    print(
                        f"\n[Disconnected: HCOM stopped for {instance_name}. Unless told otherwise, stop work and end your turn now]",
                        file=sys.stderr,
                    )
                return 0

            # Sync remote events (long-poll if backend available)
            remaining = timeout - (time.time() - start_time)
            try:
                from ..relay import relay_wait

                relay_wait(min(remaining, 25))
            except Exception:
                pass  # Best effort sync

            # Use get_unread_messages - same as real instances (handles broadcasts, @mentions, subscriptions)
            messages, _ = get_unread_messages(instance_name, update_position=True)
            if messages:
                # Adhoc: set inactive with context (we don't know what happens after)
                # Codex: set active:deliver (matches Gemini pattern - notify hook sets idle)
                # Others: set active (they have hooks to track state)
                if instance_data.get("tool") == "adhoc":
                    set_status(instance_name, "inactive", "message received")
                elif instance_data.get("tool") == "codex":
                    msg_ts = messages[-1].get("timestamp", "")
                    set_status(
                        instance_name,
                        "active",
                        f"deliver:{messages[0]['from']}",
                        msg_ts=msg_ts,
                    )
                else:
                    set_status(instance_name, "active", "finished listening")

                if json_output:
                    import json

                    for msg in messages:
                        print(
                            json.dumps(
                                {
                                    "from": msg["from"],
                                    "text": msg[
                                        "message"
                                    ],  # get_unread_messages() uses 'message' key
                                }
                            )
                        )
                else:
                    # Default: JSON format for model consumption
                    formatted = format_messages_json(messages, instance_name)
                    print(f"\n{formatted}")
                return 0

            # Update heartbeat
            _update_heartbeat(instance_name)

            # TCP select for local notifications
            remaining = timeout - (time.time() - start_time)
            if remaining <= 0:
                break
            if is_relay_enabled():
                wait_time = min(remaining, 1.0)
            elif notify_server:
                wait_time = min(remaining, 30.0)
            else:
                wait_time = min(remaining, 0.1)

            if notify_server:
                readable, _, _ = select.select([notify_server], [], [], wait_time)
                if readable:
                    _drain_notify_server(notify_server)
            else:
                time.sleep(wait_time)

        # Timeout
        # Only set inactive for adhoc instances - others have their own lifecycle
        if instance_data.get("tool") == "adhoc":
            set_status(instance_name, "inactive", "exit:timeout")
        if not json_output:
            print(f"\n[Timeout: no messages after {timeout}s]", file=sys.stderr)
        # Timeout is normal for listen (especially for external tools)
        return 0
    except KeyboardInterrupt:
        if not json_output:
            print("\n[Interrupted]", file=sys.stderr)
        return 130
    finally:
        # Clean up notify endpoint so future sends don't hit stale port
        try:
            from ..core.db import delete_notify_endpoint

            delete_notify_endpoint(instance_name, kind="listen")
        except Exception:
            pass
        if notify_server:
            try:
                notify_server.close()
            except Exception:
                pass
