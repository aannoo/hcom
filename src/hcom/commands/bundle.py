"""Bundle commands for HCOM."""

from __future__ import annotations

import json
import sys
from datetime import datetime, timezone
from typing import Any

from .utils import format_error, validate_flags, parse_flag_value, parse_flag_bool, parse_last_flag, get_command_help, CLIError
from ..shared import CommandContext, format_age, parse_iso_timestamp
from ..core.bundles import parse_csv_list, get_bundle_instance_name
from ..core.detail_levels import is_full_output_detail
from ..core.filters import FILE_OP_CONTEXTS, FILE_OP_CONTEXTS_SQL


def _get_bundle_by_id(conn, bundle_id_or_prefix: str) -> dict[str, Any] | None:
    """Fetch a bundle by bundle_id prefix or exact event id."""
    if bundle_id_or_prefix.isdigit():
        row = conn.execute(
            "SELECT id, timestamp, data FROM events WHERE type = 'bundle' AND id = ?",
            (int(bundle_id_or_prefix),),
        ).fetchone()
        return dict(row) if row else None

    # Normalize: if prefix doesn't start with "bundle:", add it
    search_prefix = bundle_id_or_prefix if bundle_id_or_prefix.startswith("bundle:") else f"bundle:{bundle_id_or_prefix}"

    row = conn.execute(
        """
        SELECT id, timestamp, data
        FROM events
        WHERE type = 'bundle'
          AND json_extract(data, '$.bundle_id') LIKE ?
        ORDER BY id DESC LIMIT 1
        """,
        (f"{search_prefix}%",),
    ).fetchone()
    return dict(row) if row else None


def _format_event_compact(event: dict) -> str:
    """Format event as compact single line for bundle prepare.

    Format: ID | type:context | agent | summary
    Examples:
      1004 | life:ready     | kazu | by user, start
      979  | status:active  | kira | tool:Bash "git status"
      980  | msg:broadcast  | kira | "checking status..." → kazu,hiro
    """
    data = json.loads(event["data"]) if isinstance(event["data"], str) else event["data"]
    etype = event["type"]
    agent = event["instance"]
    eid = event["id"]

    if etype == "life":
        action = data.get("action", "?")
        by = data.get("by", "")
        ctx = data.get("context", "")
        parts = [f"by {by}" if by else None, ctx if ctx else None]
        summary = ", ".join(p for p in parts if p) or "-"
        return f"{eid:4} | life:{action:<8} | {agent:<6} | {summary}"

    elif etype == "status":
        status = data.get("status", "?")
        ctx = data.get("context", "")
        detail = data.get("detail", "")

        # Special handling for file operations - show parent/filename
        if ctx in FILE_OP_CONTEXTS:
            op = ctx.split(":")[-1].lower()  # edit, write, read
            if detail and "/" in detail:
                parts = detail.split("/")
                # Show parent/filename (last 2 components)
                short_path = "/".join(parts[-2:]) if len(parts) >= 2 else parts[-1]
                if len(short_path) > 40:
                    short_path = ".../" + parts[-1][:35]
                return f"{eid:4} | {op:<8} | {agent:<6} | {short_path}"
            return f"{eid:4} | {op:<8} | {agent:<6} | {detail or '-'}"

        # Truncate long details (commands)
        if detail and len(detail) > 40:
            detail = detail[:37] + "..."
        summary = f'{ctx} "{detail}"' if detail else ctx or "-"
        return f"{eid:4} | {status:<8} | {agent:<6} | {summary}"

    elif etype == "message":
        sender = data.get("from", "?")
        text = data.get("text", "")
        delivered = data.get("delivered_to", [])
        scope = data.get("scope", "")
        # Truncate message preview
        preview = text[:30] + "..." if len(text) > 30 else text
        preview = preview.replace("\n", " ")
        targets = ",".join(delivered[:3]) if delivered else scope
        if len(delivered) > 3:
            targets += f"+{len(delivered)-3}"
        return f'{eid:4} | msg       | {sender:<6} | "{preview}" → {targets}'

    else:
        # Generic fallback
        return f"{eid:4} | {etype:<9} | {agent:<6} | {json.dumps(data)[:50]}"


def _query_bundle_events(
    conn,
    agent_name: str,
    last_events: int
) -> dict[str, list[dict[str, Any]]]:
    """Query events by category for bundle preparation.

    Returns dict with keys: messages_to, messages_from, file_operations, lifecycle
    """
    event_categories = {}

    # Messages TO this agent
    msgs_to = conn.execute(
        """
        SELECT id, timestamp, type, instance, data
        FROM events
        WHERE type = 'message'
          AND json_extract(data, '$.delivered_to') LIKE ?
        ORDER BY id DESC
        LIMIT ?
        """,
        (f'%"{agent_name}"%', last_events),
    ).fetchall()
    if msgs_to:
        event_categories["messages_to"] = [dict(row) for row in msgs_to]

    # Messages FROM this agent
    msgs_from = conn.execute(
        """
        SELECT id, timestamp, type, instance, data
        FROM events
        WHERE type = 'message'
          AND json_extract(data, '$.from') = ?
        ORDER BY id DESC
        LIMIT ?
        """,
        (agent_name, last_events),
    ).fetchall()
    if msgs_from:
        event_categories["messages_from"] = [dict(row) for row in msgs_from]

    # File operations (Write, Edit, Read)
    file_ops = conn.execute(
        f"""
        SELECT id, timestamp, type, instance, data
        FROM events
        WHERE type = 'status'
          AND instance = ?
          AND json_extract(data, '$.context') IN {FILE_OP_CONTEXTS_SQL}
        ORDER BY id DESC
        LIMIT ?
        """,
        (agent_name, last_events),
    ).fetchall()
    if file_ops:
        event_categories["file_operations"] = [dict(row) for row in file_ops]

    # Lifecycle events
    life_events = conn.execute(
        """
        SELECT id, timestamp, type, instance, data
        FROM events
        WHERE type = 'life'
          AND (instance = ? OR json_extract(data, '$.by') = ?)
        ORDER BY id DESC
        LIMIT ?
        """,
        (agent_name, agent_name, 5),  # Only 5 lifecycle events
    ).fetchall()
    if life_events:
        event_categories["lifecycle"] = [dict(row) for row in life_events]

    return event_categories


def _parse_prepare_flags(argv: list[str]) -> tuple[str | None, int, int, bool]:
    """Parse bundle prepare flags.

    Returns:
        (target_agent, last_transcript, last_events, compact)

    Raises:
        ValueError: If flag values are invalid
    """
    target_agent = None
    last_transcript = 40
    last_events = 10  # Per category
    compact = False

    i = 0
    while i < len(argv):
        if argv[i] == "--for" and i + 1 < len(argv):
            target_agent = argv[i + 1]
            i += 2
        elif argv[i] == "--last-transcript" and i + 1 < len(argv):
            try:
                last_transcript = int(argv[i + 1])
            except ValueError:
                raise ValueError("--last-transcript must be an integer (e.g., --last-transcript 20)")
            i += 2
        elif argv[i] == "--last-events" and i + 1 < len(argv):
            try:
                last_events = int(argv[i + 1])
            except ValueError:
                raise ValueError("--last-events must be an integer (e.g., --last-events 10)")
            i += 2
        elif argv[i] == "--compact":
            compact = True
            i += 1
        else:
            i += 1

    return target_agent, last_transcript, last_events, compact


def _collapse_file_ops(events: list[dict]) -> list[tuple[str, str, list[int]]]:
    """Collapse consecutive file operations on same file.

    Returns list of (path, op, [event_ids]) tuples.
    """
    if not events:
        return []

    collapsed: list[tuple[str, str, list[int]]] = []
    current_file: str | None = None
    current_ids: list[int] = []
    current_op: str | None = None

    for event in events:
        data = json.loads(event["data"]) if isinstance(event["data"], str) else event["data"]
        ctx = data.get("context", "")
        detail = data.get("detail", "")
        eid = event["id"]

        # Extract file path
        if detail and "/" in detail:
            parts = detail.split("/")
            short_path = "/".join(parts[-2:]) if len(parts) >= 2 else parts[-1]
        else:
            short_path = detail or "-"

        op = ctx.split(":")[-1].lower() if ctx else "?"

        if short_path == current_file and op == current_op:
            current_ids.append(eid)
        else:
            if current_file and current_op:
                collapsed.append((current_file, current_op, current_ids))
            current_file = short_path
            current_op = op
            current_ids = [eid]

    if current_file and current_op:
        collapsed.append((current_file, current_op, current_ids))

    return collapsed


def _print_prepare_output(
    agent_name: str,
    transcript_text: str | None,
    transcript_range: str | None,
    event_categories: dict[str, list[dict]],
    files_list: list[str],
    template_command: str,
    last_transcript: int,
    last_events: int,
    compact: bool = False,
) -> None:
    """Print concise bundle prepare output for AI consumption."""
    _ = transcript_range, last_transcript, last_events  # unused but kept for signature compat
    SEP = "─" * 40

    print(f"[Bundle Context: {agent_name}]\n")

    if not compact:
        print(SEP)
        print("HOW TO USE THIS CONTEXT:")
        print()
        print("Use 'hcom send' with these bundle flags to create and send directly")
        print("Transcript detail: normal (truncated) | full (complete text) | detailed (complete text with tools)")
        print()
        print("Use this bundle context as a template for your specific bundle")
        print("- Pick relevant events/files/transcript ranges from the bundle context")
        print("- Use the hcom events and hcom transcript commands to find all everything relevant to include")
        print(
            "- Specify the correct transcript detail for each transcript range "
            "(ie full when all relevant, normal only when the above is sufficient)"
        )
        print(
            "- For description: give comprehensive detail and prescision. explain what is in this bundle, "
            "summerise specific transcript ranges and events. give deep insight so another agent can understand "
            "everything you know about this. what happened, decisions, current state, issues, plans, etc."
        )
        print()
        print("A good bundle includes everything relevant and nothing irrelevant.")
        print()
        print(f"View: hcom transcript {agent_name} [--range N-N] [--full|--detailed]")
        print(f"View: hcom events {agent_name} [--last N]")
        print()
        print("Use hcom bundle prepare --compact to hide this how to section")
        print()

    # Transcript
    print(SEP)
    print("TRANSCRIPT")
    if transcript_text:
        print(transcript_text)
    else:
        print("(none)")
    print()

    # Events - compact, collapsed
    has_events = any(event_categories.get(k) for k in ["messages_to", "messages_from", "file_operations", "lifecycle"])
    if has_events:
        print(SEP)
        print("EVENTS")

        if event_categories.get("messages_to"):
            print("  Messages TO:")
            for event in event_categories["messages_to"]:
                print(f"    {_format_event_compact(event)}")

        if event_categories.get("messages_from"):
            print("  Messages FROM:")
            for event in event_categories["messages_from"]:
                print(f"    {_format_event_compact(event)}")

        if event_categories.get("file_operations"):
            print("  File operations:")
            collapsed = _collapse_file_ops(event_categories["file_operations"])
            for path, op, ids in collapsed:
                if len(ids) == 1:
                    print(f"    {ids[0]} | {op:<6} | {path}")
                else:
                    id_range = f"{ids[-1]}-{ids[0]}"  # ids are desc order
                    print(f"    {id_range} | {op:<6} | {path} (x{len(ids)})")

        if event_categories.get("lifecycle"):
            print("  Lifecycle:")
            for event in event_categories["lifecycle"]:
                print(f"    {_format_event_compact(event)}")

        print()

    # Files - relative paths
    if files_list:
        print(SEP)
        print(f"FILES ({len(files_list)})")
        for f in files_list[:20]:
            # Show relative-ish path
            parts = f.split("/")
            short = "/".join(parts[-3:]) if len(parts) > 3 else f
            print(f"  {short}")
        if len(files_list) > 20:
            print(f"  +{len(files_list) - 20} more")
        print()

    # Template command
    print(SEP)
    print("CREATE:")
    print(template_command)



def _bundle_prepare(argv: list[str], *, ctx: CommandContext | None = None, json_out: bool = False, conn: Any) -> int:
    """Prepare bundle by showing actual context: transcript text, event details, files.

    Usage: hcom bundle prepare [--for AGENT] [--last-transcript N] [--last-events N]

    Shows:
    - Actual transcript text (last N entries)
    - Actual event details in NDJSON format by category
    - Files from file operations
    - Template command to create the bundle
    """
    from ..core.identity import resolve_identity
    from ..core.instances import load_instance_position
    from ..core.transcript import get_thread, format_thread

    # Parse flags
    try:
        target_agent, last_transcript, last_events, compact = _parse_prepare_flags(argv)
    except ValueError as e:
        print(format_error(str(e)), file=sys.stderr)
        return 1

    # Resolve identity
    identity = ctx.identity if ctx else None
    if identity is None and target_agent:
        try:
            identity = resolve_identity(name=target_agent)
        except Exception:
            identity = None
    elif identity is None:
        try:
            identity = resolve_identity()
        except Exception:
            identity = None

    if identity is None or identity.kind != "instance":
        print(format_error("Cannot prepare bundle: no active agent identity found"), file=sys.stderr)
        print("Specify an agent: hcom bundle prepare --for <agent_name>", file=sys.stderr)
        return 1

    agent_name = identity.name
    instance_data = load_instance_position(agent_name)
    if not instance_data:
        print(format_error(f"Agent '{agent_name}' not found. Check active agents with: hcom list"), file=sys.stderr)
        return 1

    # Get transcript path
    transcript_path = instance_data.get("transcript_path")
    tool = instance_data.get("tool", "claude")

    # --- GET ACTUAL TRANSCRIPT TEXT ---
    transcript_text = None
    transcript_range = None
    total_entries = 0

    if transcript_path:
        try:
            thread = get_thread(transcript_path, last=last_transcript, tool=tool)
            if thread and not thread.get("error"):
                exchanges = thread.get("exchanges", [])
                total_entries = thread.get("total", 0)
                if exchanges:
                    transcript_text = format_thread(thread, instance=agent_name, full=False)
                    # Calculate range from first to last exchange position
                    first_pos = exchanges[0].get("position", 1)
                    last_pos = exchanges[-1].get("position", total_entries)
                    transcript_range = f"{first_pos}-{last_pos}"
            elif thread and thread.get("error"):
                transcript_text = f"Error reading transcript: {thread['error']}"
        except (OSError, ValueError) as e:
            transcript_text = f"Error reading transcript: {e}"

    # --- GET ACTUAL EVENTS BY CATEGORY ---
    event_categories = _query_bundle_events(conn, agent_name, last_events)

    # --- EXTRACT FILES FROM FILE OPERATIONS ---
    files_set = set()
    if "file_operations" in event_categories:
        for event in event_categories["file_operations"]:
            try:
                data = json.loads(event["data"]) if isinstance(event["data"], str) else event["data"]
                file_path = data.get("detail")
                if file_path and (
                    "/" in file_path or any(file_path.endswith(ext) for ext in [".py", ".ts", ".js", ".md", ".json"])
                ):
                    files_set.add(file_path)
            except Exception:
                pass

    files_list = sorted(list(files_set))

    # --- GENERATE TEMPLATE COMMAND ---
    all_event_ids = []
    for events in event_categories.values():
        all_event_ids.extend([e["id"] for e in events])

    template_parts = [
        f'hcom bundle create "Bundle Title Here" --name {agent_name}',
        '--description "detailed description text here"',
    ]

    if transcript_range:
        template_parts.append(f'--transcript "{transcript_range}:normal    //can be multiple: 3-14:normal,6:full,22-30:detailed"')

    if all_event_ids:
        # Take latest 20
        latest_events = sorted(all_event_ids, reverse=True)[:20]
        template_parts.append(f'--events "{",".join(map(str, latest_events))}"')

    if files_list:
        files_sample = files_list[:10]
        template_parts.append(f'--files "{",".join(files_sample)}"')

    template_command = " \\\n  ".join(template_parts)

    # --- OUTPUT ---
    if json_out:
        result = {
            "agent": agent_name,
            "transcript": {"text": transcript_text, "range": transcript_range, "total_entries": total_entries},
            "events": event_categories,
            "files": files_list,
            "template_command": template_command,
            "note": f"Last {last_transcript} transcript entries, {last_events} events per category",
        }
        print(json.dumps(result, indent=2))
        return 0

    # Human-readable output
    _print_prepare_output(
        agent_name,
        transcript_text,
        transcript_range,
        event_categories,
        files_list,
        template_command,
        last_transcript,
        last_events,
        compact,
    )
    return 0


def _bundle_cat(argv: list[str], *, conn: Any) -> int:
    """Expand and display full bundle content.

    Usage: hcom bundle cat <id>

    Shows:
    - Metadata (title, description, author)
    - Files (metadata only: path, lines, size, modified time)
    - Transcript (respects detail level: normal=truncated, full/detailed=complete)
    - Events (full event JSON)
    """
    from pathlib import Path
    from ..core.transcript import get_thread, format_thread
    from ..core.instances import load_instance_position

    # Get bundle ID
    if not argv:
        print(format_error("bundle cat requires a bundle ID (e.g., bundle:abc123 or event ID)"), file=sys.stderr)
        return 1

    bundle_id = argv[0]
    row = _get_bundle_by_id(conn, bundle_id)
    if not row:
        print(format_error(f"Bundle not found: {bundle_id}\nList bundles with: hcom bundle list"), file=sys.stderr)
        return 1

    data = json.loads(row["data"]) if row["data"] else {}

    # Extract bundle components
    title = data.get("title", "")
    description = data.get("description", "")
    created_by = data.get("created_by", "?")
    refs = data.get("refs", {})
    files = refs.get("files", [])
    transcript_refs = refs.get("transcript", [])
    event_refs = refs.get("events", [])

    SEP = "━" * 80

    # === METADATA ===
    print(f"[Bundle {data.get('bundle_id', bundle_id)}]")
    print(f"Title: {title}")
    print(f"Description: {description}")
    print(f"Created by: {created_by}")
    print()

    # === FILES (metadata only) ===
    if files:
        print(f"{SEP}")
        print(f"FILES ({len(files)})")
        print(f"{SEP}")
        print()

        for file_path in files:
            try:
                path = Path(file_path)
                if path.exists():
                    stat = path.stat()
                    lines = sum(1 for _ in path.open('r', encoding='utf-8', errors='ignore'))
                    size_kb = stat.st_size / 1024

                    # Format size
                    if size_kb < 1:
                        size_str = f"{stat.st_size} B"
                    elif size_kb < 1024:
                        size_str = f"{size_kb:.1f} KB"
                    else:
                        size_str = f"{size_kb/1024:.1f} MB"

                    # Time since modified
                    import time
                    modified_ago = time.time() - stat.st_mtime
                    if modified_ago < 3600:
                        modified_str = f"{int(modified_ago/60)}m ago"
                    elif modified_ago < 86400:
                        modified_str = f"{int(modified_ago/3600)}h ago"
                    else:
                        modified_str = f"{int(modified_ago/86400)}d ago"

                    print(f"{file_path} ({lines} lines, {size_str}, modified {modified_str})")
                else:
                    print(f"{file_path} (not found)")
            except OSError as e:
                print(f"{file_path} (error: {e})")
        print()

    # === TRANSCRIPT ===
    if transcript_refs:
        print(f"{SEP}")
        print(f"TRANSCRIPT ({len(transcript_refs)} entries)")
        print(f"{SEP}")
        print()

        # Get instance data for transcript path
        instance_data = load_instance_position(created_by)
        transcript_path = instance_data.get("transcript_path") if instance_data else None
        if transcript_path:
            tool = instance_data.get("tool", "claude") if instance_data else "claude"

            for ref in transcript_refs:
                if isinstance(ref, dict):
                    range_str = ref["range"]
                    detail = ref["detail"]
                else:
                    # Shouldn't happen after validation
                    range_str = str(ref).split(":")[0] if ":" in str(ref) else str(ref)
                    detail = "normal"

                print(f"--- Transcript [{range_str}] ({detail}) ---")
                print()

                try:
                    # Parse range
                    if "-" in range_str:
                        start, end = map(int, range_str.split("-"))
                    else:
                        start = end = int(range_str)

                    # Get transcript
                    detailed = detail == "detailed"
                    thread = get_thread(
                        transcript_path,
                        range_tuple=(start, end),
                        tool=tool,
                        detailed=detailed
                    )

                    if thread and not thread.get("error"):
                        # Format based on detail level (normal=truncated, full/detailed=complete)
                        full = is_full_output_detail(detail)
                        formatted = format_thread(thread, instance=created_by, full=full)

                        print(formatted)
                    else:
                        error = thread.get("error", "Unknown error") if thread else "Failed to read transcript"
                        print(f"Error: {error}")
                except (OSError, ValueError) as e:
                    print(f"Error reading transcript: {e}")

                print()
        else:
            print(f"Transcript unavailable: agent '{created_by}' not found or has no transcript")
            print()

    # === EVENTS ===
    if event_refs:
        print(f"{SEP}")
        print(f"EVENTS ({len(event_refs)})")
        print(f"{SEP}")
        print()

        # Parse event refs (can be individual IDs or ranges like "100-105")
        event_ids: list[int] = []
        for ref in event_refs:
            ref_str = str(ref)
            if "-" in ref_str:
                # Range
                try:
                    start, end = map(int, ref_str.split("-"))
                    event_ids.extend(range(start, end + 1))
                except ValueError:
                    event_ids.append(ref)
            else:
                event_ids.append(ref)

        # Query events
        placeholders = ",".join("?" * len(event_ids))
        rows = conn.execute(
            f"""
            SELECT id, timestamp, type, instance, data
            FROM events
            WHERE id IN ({placeholders})
            ORDER BY id ASC
            """,
            event_ids
        ).fetchall()

        for row in rows:
            event_data = {
                "id": row["id"],
                "timestamp": row["timestamp"],
                "type": row["type"],
                "instance": row["instance"],
                "data": json.loads(row["data"]) if row["data"] else {}
            }

            # Format as JSON
            json_str = json.dumps(event_data, indent=2)
            print(json_str)
            print()

    return 0


def cmd_bundle(argv: list[str], *, ctx: CommandContext | None = None) -> int:
    """Manage bundles: hcom bundle [list|show|create|chain|prepare]"""
    from ..core.db import get_db, init_db
    from ..core.bundles import create_bundle_event, validate_bundle
    from ..core.identity import resolve_identity

    init_db()

    # Default subcommand: list
    argv = argv.copy()
    subcmd = "list"
    if argv and not argv[0].startswith("-"):
        subcmd = argv.pop(0)

    if subcmd not in {"list", "show", "create", "chain", "prepare", "preview", "cat"}:
        print(format_error(f"Unknown bundle subcommand: {subcmd}"), file=sys.stderr)
        print(get_command_help("bundle"), file=sys.stderr)
        return 1

    # Validate flags
    if error := validate_flags(f"bundle {subcmd}", argv):
        print(format_error(error), file=sys.stderr)
        return 1

    # Common flags (removes from argv)
    json_out, argv = parse_flag_bool(argv, "--json")

    conn = get_db()

    # Handle prepare/preview subcommand
    if subcmd in {"prepare", "preview"}:
        return _bundle_prepare(argv, ctx=ctx, json_out=json_out, conn=conn)

    # Handle cat subcommand
    if subcmd == "cat":
        return _bundle_cat(argv, conn=conn)

    if subcmd == "list":
        try:
            last, argv = parse_last_flag(argv, default=20)
        except CLIError as e:
            print(format_error(str(e)), file=sys.stderr)
            return 1

        rows = conn.execute(
            """
            SELECT id, timestamp,
                   json_extract(data, '$.bundle_id') as bundle_id,
                   json_extract(data, '$.title') as title,
                   json_extract(data, '$.description') as description,
                   json_extract(data, '$.created_by') as created_by,
                   json_extract(data, '$.refs.events') as events
            FROM events
            WHERE type = 'bundle'
            ORDER BY id DESC
            LIMIT ?
            """,
            (last,),
        ).fetchall()

        bundles = []
        for r in rows:
            bundles.append(
                {
                    "id": r["id"],
                    "timestamp": r["timestamp"],
                    "bundle_id": r["bundle_id"],
                    "title": r["title"],
                    "description": r["description"],
                    "created_by": r["created_by"],
                    "events": json.loads(r["events"]) if r["events"] else [],
                }
            )

        if json_out:
            print(json.dumps(bundles))
            return 0

        if not bundles:
            print("No bundles found. Create one with: hcom bundle prepare")
            return 0

        for b in bundles:
            age = ""
            if b["timestamp"]:
                dt = parse_iso_timestamp(b["timestamp"])
                if dt:
                    seconds_ago = (datetime.now(timezone.utc) - dt).total_seconds()
                    age = format_age(seconds_ago)
            events_count = len(b["events"])
            created_by = b["created_by"] or "?"
            bundle_id = b["bundle_id"] or f"event:{b['id']}"
            print(f"{bundle_id} | {b['title']} | {created_by} | {events_count} events | {age}")
        return 0

    if subcmd == "show":
        if not argv:
            print(format_error("bundle show requires an id"), file=sys.stderr)
            return 1
        bundle_id = argv[0]
        row = _get_bundle_by_id(conn, bundle_id)
        if not row:
            print(format_error(f"Bundle not found: {bundle_id}"), file=sys.stderr)
            return 1
        data = json.loads(row["data"]) if row["data"] else {}
        data["event_id"] = row["id"]
        data["timestamp"] = row["timestamp"]
        if json_out:
            print(json.dumps(data))
            return 0
        print(json.dumps(data, indent=2))
        return 0

    if subcmd == "chain":
        if not argv:
            print(format_error("bundle chain requires an id"), file=sys.stderr)
            return 1
        bundle_id = argv[0]
        chain = []
        current = _get_bundle_by_id(conn, bundle_id)
        if not current:
            print(format_error(f"Bundle not found: {bundle_id}"), file=sys.stderr)
            return 1
        while current:
            data = json.loads(current["data"]) if current["data"] else {}
            data["event_id"] = current["id"]
            data["timestamp"] = current["timestamp"]
            chain.append(data)
            extends = data.get("extends")
            if not extends:
                break
            current = _get_bundle_by_id(conn, extends)
            if not current:
                print(f"Warning: missing ancestor bundle {extends}", file=sys.stderr)
                break

        if json_out:
            print(json.dumps(chain))
            return 0

        for i, b in enumerate(chain):
            prefix = "  ↳ " * i
            print(f'{prefix}{b.get("bundle_id", "")} "{b.get("title", "")}"')
        return 0

    # subcmd == "create"
    # Require identity for create
    identity = ctx.identity if ctx else None
    if identity is None:
        try:
            identity = resolve_identity()
        except Exception:
            identity = None
    if identity is None:
        print(format_error("Cannot create bundle without identity"), file=sys.stderr)
        return 1

    # Optional JSON payload
    bundle = None
    if "--bundle" in argv:
        idx = argv.index("--bundle")
        if idx + 1 >= len(argv) or argv[idx + 1].startswith("-"):
            print(format_error("--bundle requires JSON value"), file=sys.stderr)
            return 1
        raw = argv[idx + 1]
        try:
            bundle = json.loads(raw)
        except json.JSONDecodeError as e:
            print(format_error(f"Invalid --bundle JSON: {e}"), file=sys.stderr)
            return 1
        argv = argv[:idx] + argv[idx + 2 :]

    # Parse --bundle-file using parse_flag_value
    try:
        from pathlib import Path
        bundle_file_path, argv = parse_flag_value(argv, "--bundle-file", required=True)
        if bundle_file_path:
            if bundle is not None:
                print(
                    format_error("--bundle and --bundle-file are mutually exclusive"),
                    file=sys.stderr,
                )
                return 1
            path = Path(bundle_file_path)
            try:
                raw = path.read_text(encoding="utf-8")
                bundle = json.loads(raw)
            except OSError as e:
                print(format_error(f"Failed to read --bundle-file: {e}"), file=sys.stderr)
                return 1
            except json.JSONDecodeError as e:
                print(format_error(f"Invalid --bundle-file JSON: {e}"), file=sys.stderr)
                return 1
    except (OSError, json.JSONDecodeError):
        # Flag not present
        pass

    if bundle is not None:
        try:
            hints = validate_bundle(bundle)
        except ValueError as e:
            print(format_error(str(e)), file=sys.stderr)
            return 1
        if hints:
            print("Bundle quality hints:", file=sys.stderr)
            for h in hints:
                print(f"  - {h}", file=sys.stderr)
    else:
        title = None
        if argv and not argv[0].startswith("-"):
            title = argv[0]
            argv = argv[1:]
        if "--title" in argv:
            idx = argv.index("--title")
            if idx + 1 >= len(argv) or argv[idx + 1].startswith("-"):
                print(format_error("--title requires a value"), file=sys.stderr)
                return 1
            title_flag = argv[idx + 1]
            argv = argv[:idx] + argv[idx + 2 :]
            if title and title != title_flag:
                print(format_error("Title provided twice with different values"), file=sys.stderr)
                return 1
            title = title_flag
        if not title:
            print(format_error("bundle create requires a title"), file=sys.stderr)
            return 1

        if "--description" not in argv:
            print(format_error("--description is required. Give a comprehensive detailed description."), file=sys.stderr)
            return 1
        idx = argv.index("--description")
        if idx + 1 >= len(argv) or argv[idx + 1].startswith("-"):
            print(format_error("--description requires a value"), file=sys.stderr)
            return 1
        description = argv[idx + 1]
        argv = argv[:idx] + argv[idx + 2 :]

        events = []
        files = []
        transcript = []
        extends = None

        if "--events" in argv:
            idx = argv.index("--events")
            if idx + 1 >= len(argv) or argv[idx + 1].startswith("-"):
                print(format_error("--events requires event IDs (e.g., --events \"123,124\" or --events \"100-105\")"), file=sys.stderr)
                return 1
            events = parse_csv_list(argv[idx + 1])
            argv = argv[:idx] + argv[idx + 2 :]
        if "--files" in argv:
            idx = argv.index("--files")
            if idx + 1 >= len(argv) or argv[idx + 1].startswith("-"):
                print(format_error("--files requires file paths (e.g., --files \"src/main.py,tests/test.py\")"), file=sys.stderr)
                return 1
            files = parse_csv_list(argv[idx + 1])
            argv = argv[:idx] + argv[idx + 2 :]
        if "--transcript" in argv:
            idx = argv.index("--transcript")
            if idx + 1 >= len(argv) or argv[idx + 1].startswith("-"):
                print(format_error("--transcript requires ranges with detail (e.g., --transcript \"1-5:normal,10:full\")"), file=sys.stderr)
                return 1
            transcript = parse_csv_list(argv[idx + 1])
            argv = argv[:idx] + argv[idx + 2 :]
        if "--extends" in argv:
            idx = argv.index("--extends")
            if idx + 1 >= len(argv) or argv[idx + 1].startswith("-"):
                print(format_error("--extends requires a value"), file=sys.stderr)
                return 1
            extends = argv[idx + 1]
            argv = argv[:idx] + argv[idx + 2 :]

        bundle = {
            "title": title,
            "description": description,
            "refs": {"events": events, "files": files, "transcript": transcript},
        }
        if extends:
            bundle["extends"] = extends

    bundle_instance = get_bundle_instance_name(identity)

    bundle_id = create_bundle_event(bundle, instance=bundle_instance, created_by=identity.name)
    try:
        from ..relay import trigger_push

        trigger_push()
    except Exception:
        pass
    result = {"bundle_id": bundle_id}
    if json_out:
        print(json.dumps(result))
        return 0
    print(bundle_id)
    return 0


__all__ = ["cmd_bundle"]
