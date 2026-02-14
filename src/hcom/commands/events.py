"""Events commands for HCOM"""

import sqlite3
import sys
import json
import time
from datetime import datetime
from .utils import format_error, parse_flag_bool, parse_last_flag, CLIError
from ..shared import CommandContext, HcomError


def streamline_event(event: dict, filters: dict | None = None) -> dict:
    """Remove bloat fields from event, keep useful data.

    Achieves ~35% token reduction while maintaining all useful information.
    Dropped fields are redundant or rarely used by agents.

    If filters are provided, fields used in filtering are preserved fully
    (not truncated/stripped) since the user explicitly queried them.
    """
    filters = filters or {}
    data = event.get("data", {}).copy()

    # Drop universal bloat fields
    data.pop("sender_kind", None)      # Always "instance" for agent-to-agent
    data.pop("scope", None)            # Always "mentions" (inferred from routing)
    data.pop("delivered_to", None)     # Agents don't use this (internal routing)
    # Keep mentions if filtered by --mention
    if "mention" not in filters:
        data.pop("mentions", None)

    # Drop type-specific bloat
    if event["type"] == "message":
        data.pop("reply_to", None)     # Duplicate of reply_to_local

    elif event["type"] == "status":
        # --cmd or --file filter on detail - keep full if filtered, else truncate
        if "cmd" in filters or "file" in filters:
            pass  # Keep detail as-is
        elif "detail" in data and data["detail"] and len(data["detail"]) > 60:
            data["detail"] = data["detail"][:60] + "..."
        data.pop("position", None)     # Internal watermark (not for agents)

    elif event["type"] == "life":
        data.pop("snapshot", None)     # Huge nested object (use --full if needed)

    # Truncate timestamp microseconds (second precision is sufficient)
    timestamp = event.get("ts", "")
    if len(timestamp) > 19:
        timestamp = timestamp[:19]

    return {
        "id": event["id"],
        "ts": timestamp,
        "type": event["type"],
        "instance": event["instance"],
        "data": data,
    }


def _cmd_events_launch(argv: list[str], instance_name: str | None = None) -> int:
    """Wait for launches ready, output JSON. Internal - called by launch output."""
    from ..core.db import init_db
    from ..core.launch_status import wait_for_launch
    from .utils import resolve_identity, validate_flags

    # Validate flags
    if error := validate_flags("events launch", argv):
        print(format_error(error), file=sys.stderr)
        return 1

    init_db()

    # Parse --timeout flag
    timeout = 30  # default
    if "--timeout" in argv:
        idx = argv.index("--timeout")
        argv = argv[:idx] + argv[idx + 1:]
        if idx < len(argv) and not argv[idx].startswith("-"):
            try:
                timeout = int(argv[idx])
                argv = argv[:idx] + argv[idx + 1:]
            except ValueError:
                print(format_error(f"--timeout must be an integer, got '{argv[idx]}'"), file=sys.stderr)
                return 1

    # Parse batch_id arg (for specific batch lookup)
    batch_id = argv[0] if argv and not argv[0].startswith("--") else None

    # Find launcher identity if in AI tool context (Claude, Gemini, Codex)
    from ..shared import is_inside_ai_tool

    launcher = instance_name  # Use explicit instance_name if provided
    if not launcher and is_inside_ai_tool():
        try:
            launcher = resolve_identity().name
        except Exception:
            pass

    result = wait_for_launch(launcher=launcher, batch_id=batch_id, timeout=timeout)
    print(json.dumps(result))

    return 0 if result.get("status") == "ready" else 1


def cmd_events(argv: list[str], *, ctx: CommandContext | None = None) -> int:
    """Query events from SQLite: hcom events [launch|sub|unsub] [--last N] [--wait SEC] [--sql EXPR] [--name NAME]"""
    from ..core.db import get_db, init_db, get_last_event_id
    from .utils import parse_name_flag, validate_flags, parse_flag_value
    from ..core.identity import resolve_identity
    from ..core.filters import (
        expand_shortcuts,
        parse_event_flags,
        build_sql_from_flags,
    )

    init_db()  # Ensure schema exists

    # Identity (instance-only): CLI supplies ctx (preferred). Direct calls may still pass --name.
    from_value = ctx.explicit_name if ctx else None
    argv_parsed = argv
    if ctx is None:
        from_value, argv_parsed = parse_name_flag(argv)

    # Resolve identity if --name provided
    # caller_name: used for subscriptions (can be external name or instance name)
    # instance_name: only set for real instances (used for message delivery)
    caller_name = None
    instance_name = None
    if ctx and ctx.identity and ctx.identity.kind == "instance":
        instance_name = ctx.identity.name
        caller_name = ctx.identity.name
    elif from_value:
        try:
            identity = resolve_identity(name=from_value)
            if identity.kind == "instance":
                instance_name = identity.name
                caller_name = identity.name
        except (HcomError, ValueError, sqlite3.Error) as e:
            print(format_error(f"Cannot resolve '{from_value}': {e}"), file=sys.stderr)
            return 1

    # Handle 'launch' subcommand
    if argv_parsed and argv_parsed[0] == "launch":
        return _cmd_events_launch(argv_parsed[1:], instance_name=instance_name)

    # Handle 'sub' subcommand (list or subscribe)
    if argv_parsed and argv_parsed[0] == "sub":
        return _events_sub(argv_parsed[1:], caller_name=caller_name)

    # Handle 'unsub' subcommand (unsubscribe)
    if argv_parsed and argv_parsed[0] == "unsub":
        return _events_unsub(argv_parsed[1:], caller_name=caller_name)

    # Validate flags early (use argv_parsed which has --name removed)
    if error := validate_flags("events", argv_parsed):
        print(format_error(error), file=sys.stderr)
        return 1

    # Use already-parsed values from above
    argv = argv_parsed

    # Parse boolean flags first (removes from argv)
    search_all, argv = parse_flag_bool(argv, "--all")
    full_output, argv = parse_flag_bool(argv, "--full")

    # Parse --last flag
    try:
        last_n, argv = parse_last_flag(argv, default=20)
    except CLIError as e:
        print(format_error(str(e)), file=sys.stderr)
        return 1

    # Parse --sql flag
    try:
        sql_where, argv = parse_flag_value(argv, "--sql")
        if sql_where:
            # Fix shell escaping: bash/zsh escape ! as \! in double quotes (history expansion)
            sql_where = sql_where.replace("\\!", "!")
    except CLIError as e:
        print(format_error(str(e)), file=sys.stderr)
        return 1

    # Parse --wait flag (optional value, defaults to 60s)
    wait_timeout = None
    if "--wait" in argv:
        idx = argv.index("--wait")
        argv = argv[:idx] + argv[idx + 1:]
        # Check if next arg is a value (not a flag)
        if idx < len(argv) and not argv[idx].startswith("-"):
            try:
                wait_timeout = int(argv[idx])
                argv = argv[:idx] + argv[idx + 1:]
            except ValueError:
                print(format_error(f"--wait must be an integer, got '{argv[idx]}'"), file=sys.stderr)
                return 1
        else:
            wait_timeout = 60  # Default: 60 seconds

    # PHASE 1: Expand shortcuts FIRST (--idle, --blocked)
    argv = expand_shortcuts(argv)

    # PHASE 2: Parse filter flags
    try:
        filters, argv = parse_event_flags(argv)
    except ValueError as e:
        print(format_error(str(e)), file=sys.stderr)
        return 1

    # Build base query for filters
    db = get_db()
    filter_query = ""

    # PHASE 3: Generate SQL from filter flags
    if filters:
        try:
            flag_sql = build_sql_from_flags(filters)
            if flag_sql:
                filter_query += f" AND ({flag_sql})"
        except ValueError as e:
            print(format_error(f"Filter error: {e}"), file=sys.stderr)
            return 1

    # Add user SQL WHERE clause directly (no validation needed)
    # Note: SQL injection is not a security concern in hcom's threat model.
    # User (or ai) owns ~/.hcom/hcom.db and can already run: sqlite3 ~/.hcom/hcom.db "anything"
    # Validation would block legitimate queries while providing no actual security.
    # Filters and --sql are ANDed together if both provided.
    if sql_where:
        filter_query += f" AND ({sql_where})"

    # Wait mode: block until matching event or timeout
    if wait_timeout:
        import select

        # Check for matching events in last 10s (race condition window)
        from datetime import timezone

        lookback_timestamp = datetime.fromtimestamp(time.time() - 10, tz=timezone.utc).isoformat()
        lookback_query = f"SELECT * FROM events_v WHERE timestamp > ?{filter_query} ORDER BY id DESC LIMIT 1"

        try:
            lookback_row = db.execute(lookback_query, [lookback_timestamp]).fetchone()
        except sqlite3.Error as e:
            print(f"Error in SQL WHERE clause: {e}", file=sys.stderr)
            return 2

        if lookback_row:
            try:
                event = {
                    "id": lookback_row["id"],
                    "ts": lookback_row["timestamp"],
                    "type": lookback_row["type"],
                    "instance": lookback_row["instance"],
                    "data": json.loads(lookback_row["data"]),
                }
                # Found recent matching event, return immediately
                output = event if full_output else streamline_event(event, filters)
                print(json.dumps(output))
                return 0
            except (json.JSONDecodeError, TypeError):
                pass  # Ignore corrupt event, continue to wait loop

        # Setup TCP notification for instant wake on local events
        notify_server = None
        notify_port = None
        if instance_name:
            from ..core.runtime import create_notify_server

            notify_server, notify_port = create_notify_server()
            if notify_port:
                from ..core.db import upsert_notify_endpoint

                upsert_notify_endpoint(instance_name, "events_wait", notify_port)

        start_time = time.time()
        last_id: int | str = get_last_event_id()

        try:
            while time.time() - start_time < wait_timeout:
                query = f"SELECT * FROM events_v WHERE id > ?{filter_query} ORDER BY id"

                try:
                    rows = db.execute(query, [last_id]).fetchall()
                except sqlite3.Error as e:
                    print(f"Error in SQL WHERE clause: {e}", file=sys.stderr)
                    return 2

                if rows:
                    # Process matching events
                    for row in rows:
                        try:
                            event = {
                                "id": row["id"],
                                "ts": row["timestamp"],
                                "type": row["type"],
                                "instance": row["instance"],
                                "data": json.loads(row["data"]),
                            }

                            # Event matches all conditions, print and exit
                            output = event if full_output else streamline_event(event, filters)
                            print(json.dumps(output))
                            return 0

                        except (json.JSONDecodeError, TypeError) as e:
                            # Skip corrupt events, log to stderr
                            print(
                                f"Warning: Skipping corrupt event ID {row['id']}: {e}",
                                file=sys.stderr,
                            )
                            continue

                    # All events processed, update last_id and continue waiting
                    last_id = rows[-1]["id"]

                # Check if current instance received messages (interrupt wait to notify)
                from .utils import resolve_identity
                from ..core.messages import get_unread_messages
                from ..pty.pty_common import build_listen_instruction

                # Use explicit instance_name if provided, otherwise auto-detect
                if instance_name:
                    check_instance = instance_name
                else:
                    try:
                        check_instance = resolve_identity().name
                    except Exception:
                        check_instance = None
                if check_instance:
                    messages, _ = get_unread_messages(check_instance, update_position=False)
                    if messages:
                        # Notify without marking read; delivery happens via hooks or listen
                        print(build_listen_instruction(check_instance))
                        return 0

                # Sync remote events + wait for local TCP notification
                from ..relay import relay_wait, is_relay_enabled

                remaining = wait_timeout - (time.time() - start_time)
                if remaining > 0:
                    # Short relay poll (doesn't block long), then TCP select for local wake
                    if is_relay_enabled():
                        relay_wait(min(remaining, 2))  # Short poll, don't block

                    # TCP select for instant local wake, or short sleep as fallback
                    if notify_server:
                        wait_time = min(remaining, 5.0)  # Check relay again every 5s
                        readable, _, _ = select.select([notify_server], [], [], wait_time)
                        if readable:
                            # Drain pending notifications
                            while True:
                                try:
                                    notify_server.accept()[0].close()
                                except BlockingIOError:
                                    break
                    else:
                        time.sleep(0.5)

            print(json.dumps({"timed_out": True}))
            return 1
        finally:
            if notify_server:
                try:
                    notify_server.close()
                except Exception:
                    pass
                # Clean up notify endpoint from DB to prevent stale port accumulation
                if instance_name and notify_port:
                    try:
                        from ..core.db import delete_notify_endpoint

                        delete_notify_endpoint(instance_name, kind="events_wait", port=notify_port)
                    except Exception:
                        pass

    # Snapshot mode (default)
    all_events: list[dict] = []

    # Query current session
    query = "SELECT * FROM events_v WHERE 1=1"
    query += filter_query
    query += " ORDER BY id DESC"
    query += f" LIMIT {last_n}"

    try:
        rows = db.execute(query).fetchall()
        for row in rows:
            try:
                event = {
                    "id": row["id"],
                    "ts": row["timestamp"],
                    "type": row["type"],
                    "instance": row["instance"],
                    "data": json.loads(row["data"]),
                }
                if search_all:
                    event["source"] = "current"
                all_events.append(event)
            except (json.JSONDecodeError, TypeError) as e:
                print(
                    f"Warning: Skipping corrupt event ID {row['id']}: {e}",
                    file=sys.stderr,
                )
    except sqlite3.Error as e:
        print(f"Error in SQL WHERE clause: {e}", file=sys.stderr)
        return 2

    # Query archives if --all
    if search_all:
        from .query import _list_archives, _query_archive_events

        archives = _list_archives()
        for archive in archives:
            try:
                archive_events = _query_archive_events(archive, sql_where, last_n)
                for event in archive_events:
                    event["ts"] = event.pop("timestamp")
                    event["source"] = archive["name"]
                    all_events.append(event)
            except sqlite3.Error as e:
                # Skip archives with incompatible schema or query errors
                print(
                    f"Warning: Skipping archive {archive['name']}: {e}",
                    file=sys.stderr,
                )

    # Sort by timestamp and limit
    all_events.sort(key=lambda e: e["ts"])
    if len(all_events) > last_n:
        all_events = all_events[-last_n:]

    # Output events (streamlined by default, --full for all fields)
    for event in all_events:
        if full_output:
            # Full event data (debugging, complex parsing)
            print(json.dumps(event))
        else:
            # Streamlined (default: ~35% token reduction)
            print(json.dumps(streamline_event(event, filters)))

    return 0


# ==================== Event Subscriptions ====================


def _events_sub(argv: list[str], caller_name: str | None = None, silent: bool = False) -> int:
    """Subscribe to events or list subscriptions.

    hcom events sub                             - list all subscriptions
    hcom events sub --collision                 - file collision warnings
    hcom events sub --idle peso                 - peso returns to listening
    hcom events sub --agent peso --cmd git      - peso's git commands
    hcom events sub --cmd ^py                   - commands starting with "py"
    hcom events sub --cmd =ls                   - exact command match
    hcom events sub --file '*.py' --once        - one-shot (auto-removed after match)
    hcom events sub --status blocked --for X    - subscribe on behalf of instance X

    Manual SQL (for complex queries):
    hcom events sub "type='message' AND msg_from='bigboss'"
    """
    from ..core.db import get_db, get_last_event_id, kv_set, kv_get
    from ..core.instances import load_instance_position
    from .utils import resolve_identity, validate_flags
    from hashlib import sha256
    from ..core.filters import (
        expand_shortcuts,
        parse_event_flags,
        build_sql_from_flags,
    )

    # Validate flags
    if error := validate_flags("events sub", argv):
        print(format_error(error), file=sys.stderr)
        return 1

    # PHASE 1: Expand shortcuts FIRST (--idle, --blocked)
    argv = expand_shortcuts(argv)

    # PHASE 2: Parse filter flags
    try:
        filters, argv_remaining = parse_event_flags(argv)
    except ValueError as e:
        print(format_error(str(e)), file=sys.stderr)
        return 1

    # Parse args
    once = "--once" in argv_remaining
    target_instance = None
    i = 0
    sql_parts = []
    while i < len(argv_remaining):
        if argv_remaining[i] == "--once":
            i += 1
        elif argv_remaining[i] == "--for":
            if i + 1 >= len(argv_remaining):
                print("Error: --for requires name", file=sys.stderr)
                return 1
            target_instance = argv_remaining[i + 1]
            i += 2
        elif not argv_remaining[i].startswith("-"):
            sql_parts.append(argv_remaining[i])
            i += 1
        else:
            i += 1

    conn = get_db()
    now = time.time()

    # PHASE 3: Handle filter-based subscription (NEW)
    if filters:
        # Build SQL from filters
        try:
            sql = build_sql_from_flags(filters)
            if not sql:
                print("Error: No valid filters provided", file=sys.stderr)
                return 1
        except ValueError as e:
            print(format_error(f"Filter error: {e}"), file=sys.stderr)
            return 1

        # Combine filter SQL with manual SQL if provided (consistency with cmd_events/cmd_listen)
        if sql_parts:
            manual_sql = " ".join(sql_parts).replace("\\!", "!")
            sql = f"({sql}) AND ({manual_sql})"

        # Resolve caller for subscription
        try:
            caller = caller_name if caller_name else resolve_identity().name
        except Exception:
            print(
                format_error("Cannot create subscription without identity."),
                file=sys.stderr,
            )
            print("Run 'hcom start' first, or use --name.", file=sys.stderr)
            return 1

        # COLLISION SELF-RELEVANCE: Add caller-specific filtering for collision subscriptions
        # Only notify about collisions involving the caller (either as event instance or recent editor)
        if "collision" in filters:
            from ..core.filters import FILE_WRITE_CONTEXTS, _escape_sql

            caller_escaped = _escape_sql(caller)
            self_relevance_clause = f"""(
    events_v.instance = '{caller_escaped}'
    OR EXISTS (
        SELECT 1 FROM events_v e2
        WHERE e2.type = 'status' AND e2.status_context IN {FILE_WRITE_CONTEXTS}
        AND e2.status_detail = events_v.status_detail
        AND e2.instance = '{caller_escaped}'
        AND ABS(strftime('%s', events_v.timestamp) - strftime('%s', e2.timestamp)) < 20
    )
)"""
            sql = f"({sql}) AND {self_relevance_clause}"

        # Generate subscription ID from caller + filters + full SQL
        # Include caller to scope subscription per user (prevents cross-user collisions)
        # Include sql to account for manual SQL differences (prevents same-user logic collisions)
        id_source = f"{caller}:{json.dumps(filters, sort_keys=True)}:{sql}"
        filter_hash = sha256(id_source.encode()).hexdigest()[:8]
        sub_id = f"sub-{filter_hash}"
        sub_key = f"events_sub:{sub_id}"

        # Check if already exists
        if kv_get(sub_key):
            if not silent:
                print(f"Subscription {sub_id} already exists")
            return 0

        # Store subscription with JSON filters AND SQL (runtime needs both)
        kv_set(
            sub_key,
            json.dumps(
                {
                    "id": sub_id,
                    "caller": caller,
                    "filters": filters,  # Store filters as JSON for display/debugging
                    "sql": sql,  # REQUIRED: Runtime watcher uses this to match events
                    "created": now,
                    "last_id": get_last_event_id(),
                    "once": once,
                }
            ),
        )

        if not silent:
            print(f"Subscription {sub_id} created")
            # Show what it matches
            try:
                test_count = conn.execute(f"SELECT COUNT(*) FROM events_v WHERE ({sql})").fetchone()[0]
                if test_count > 0:
                    print(f"  historical matches: {test_count} events")
            except Exception:
                pass  # Ignore errors in test query

        return 0

    # No args = list subscriptions
    if not sql_parts:
        rows = conn.execute("SELECT key, value FROM kv WHERE key LIKE 'events_sub:%'").fetchall()

        if not rows:
            print("No active subscriptions")
            return 0

        subs = []
        for row in rows:
            try:
                subs.append(json.loads(row["value"]))
            except Exception:
                pass

        if not subs:
            print("No active subscriptions")
            return 0

        print(f"{'ID':<10} {'FOR':<12} {'MODE':<10} FILTER")
        for sub in subs:
            mode = "once" if sub.get("once") else "continuous"

            # Handle both filter-based (new) and SQL-based (old) subscriptions
            if "filters" in sub:
                filter_display = json.dumps(sub["filters"])
                if len(filter_display) > 35:
                    filter_display = filter_display[:35] + "..."
            else:
                sql_display = sub.get("sql", "")
                filter_display = sql_display[:35] + "..." if len(sql_display) > 35 else sql_display

            print(f"{sub['id']:<10} {sub['caller']:<12} {mode:<10} {filter_display}")

        return 0

    # Create custom subscription (using composable filters)
    # Fix shell escaping: bash/zsh escape ! as \! in double quotes (history expansion)
    sql = " ".join(sql_parts).replace("\\!", "!")

    # Validate SQL syntax (use events_v for flat field access)
    try:
        conn.execute(f"SELECT 1 FROM events_v WHERE ({sql}) LIMIT 0")
    except Exception as e:
        print(f"Invalid SQL: {e}", file=sys.stderr)
        return 1

    # Resolve target (--for) or use caller's identity
    if target_instance:
        # Validate target instance exists
        target_data = load_instance_position(target_instance)
        if not target_data:
            # Try prefix match
            row = conn.execute(
                "SELECT name FROM instances WHERE name LIKE ? LIMIT 1",
                (f"{target_instance}%",),
            ).fetchone()
            if row:
                target_instance = row["name"]
                target_data = load_instance_position(target_instance)

        if not target_data:
            print(f"Not found: {target_instance}", file=sys.stderr)
            print("Use 'hcom list' to see available agents", file=sys.stderr)
            return 1

        caller = target_instance
    else:
        # Resolve caller - require explicit identity for subscriptions
        if caller_name:
            caller = caller_name
        else:
            try:
                caller = resolve_identity().name
            except Exception:
                print(
                    format_error("Cannot create subscription without identity. Run 'hcom start' first or use --name."),
                    file=sys.stderr,
                )
                return 1

    # Test against recent events to show what would match
    test_count = conn.execute(f"SELECT COUNT(*) FROM events_v WHERE ({sql})").fetchone()[0]

    # Generate ID
    sub_id = f"sub-{sha256(f'{caller}{sql}{now}'.encode()).hexdigest()[:4]}"

    # Store subscription
    key = f"events_sub:{sub_id}"
    value = json.dumps(
        {
            "id": sub_id,
            "sql": sql,
            "caller": caller,
            "once": once,
            "last_id": get_last_event_id(),
            "created": now,
        }
    )
    kv_set(key, value)

    # Output with validation feedback
    print(f"{sub_id}")
    print(f"  for: {caller}")
    print(f"  filter: {sql}")
    if test_count > 0:
        print(f"  historical matches: {test_count} events")
        # Show most recent match as example
        example = conn.execute(
            f"SELECT timestamp, type, instance, data FROM events_v WHERE ({sql}) ORDER BY id DESC LIMIT 1"
        ).fetchone()
        if example:
            print(f"  latest match: [{example['type']}] {example['instance']} @ {example['timestamp'][:19]}")
    else:
        print("  historical matches: 0 (filter will apply to future events only)")
        import re

        # Warn about = comparison on JSON array fields (common mistake)
        # These fields are arrays like ["name1","name2"], not strings
        array_fields = ["msg_delivered_to", "msg_mentions"]
        for field in array_fields:
            # Match patterns like: field='value' or field = 'value' (but not LIKE)
            if re.search(rf"\b{field}\s*=\s*['\"]", sql, re.IGNORECASE):
                print(f"  Warning: {field} is a JSON array - use LIKE '%name%' not ='name'")

        # Warn about json_extract paths that don't exist in recent events
        paths = re.findall(r"json_extract\s*\(\s*data\s*,\s*['\"](\$\.[^'\"]+)['\"]", sql)
        if paths:
            # Check which paths exist in recent events
            missing = []
            for path in set(paths):
                exists = conn.execute(
                    "SELECT 1 FROM events WHERE json_extract(data, ?) IS NOT NULL LIMIT 1",
                    (path,),
                ).fetchone()
                if not exists:
                    missing.append(path)
            if missing:
                print(
                    f"  Warning: field(s) not found in any events: {', '.join(missing)} \nYou should probably double check the syntax"
                )

    return 0


def _events_unsub(argv: list[str], caller_name: str | None = None) -> int:
    """Remove subscription: hcom events unsub <id|preset|preset:target>"""
    from ..core.db import get_db, kv_set
    from .utils import validate_flags

    # Validate flags
    if error := validate_flags("events unsub", argv):
        print(format_error(error), file=sys.stderr)
        return 1

    if not argv:
        print("Usage: hcom events unsub <id>", file=sys.stderr)
        return 1

    sub_id = argv[0]

    # Handle prefix match (allow 'a3f2' instead of 'sub-a3f2')
    if not sub_id.startswith("sub-"):
        sub_id = f"sub-{sub_id}"

    key = f"events_sub:{sub_id}"

    # Check exists
    conn = get_db()
    row = conn.execute("SELECT value FROM kv WHERE key = ?", (key,)).fetchone()
    if not row:
        print(f"Not found: {sub_id}", file=sys.stderr)
        print("Use 'hcom events sub' to list active subscriptions.", file=sys.stderr)
        return 1

    kv_set(key, None)
    print(f"Removed {sub_id}")
    return 0
