"""Query commands for HCOM - list and archive operations"""

import sqlite3
import sys
import os
import json
import time
from pathlib import Path
from typing import Any

from .utils import format_error, parse_flag_bool, parse_last_flag, CLIError
from ..core.paths import hcom_path, ARCHIVE_DIR
from ..core.instances import (
    get_instance_status,
    get_status_icon,
    get_full_name,
    resolve_display_name,
)
from ..shared import (
    format_age,
    shorten_path,
    HcomError,
    CommandContext,
    ST_ACTIVE,
    ST_LISTENING,
    ST_BLOCKED,
    ST_INACTIVE,
)


def _list_archives(here_filter: bool = False, limit: int = 0) -> list[dict]:
    """Get list of archive sessions with metadata.

    Args:
        here_filter: If True, only show archives with instances in current directory
        limit: Max archives to return (0 = unlimited)
    """
    from ..core.db import DB_FILE
    import sqlite3
    from hcom.core.thread_context import get_cwd

    archive_dir = hcom_path(ARCHIVE_DIR)
    if not archive_dir.exists():
        return []

    cwd = str(get_cwd()) if here_filter else None
    archives: list[dict[str, Any]] = []

    for session_dir in sorted(archive_dir.glob("session-*"), reverse=True):
        if not session_dir.is_dir():
            continue

        db_path = session_dir / DB_FILE
        if not db_path.exists():
            continue

        try:
            stat = db_path.stat()
            archive_info = {
                "index": len(archives) + 1,
                "name": session_dir.name,
                "path": str(session_dir),
                "timestamp": session_dir.name.replace("session-", ""),
                "size_bytes": stat.st_size,
                "created": stat.st_mtime,
            }

            # Get event/instance counts
            try:
                conn = sqlite3.connect(str(db_path))
                conn.row_factory = sqlite3.Row
                event_count = conn.execute("SELECT COUNT(*) FROM events").fetchone()[0]
                instance_count = conn.execute("SELECT COUNT(*) FROM instances").fetchone()[0]

                # Filter by directory if --here
                if here_filter and cwd:
                    dir_match = conn.execute("SELECT 1 FROM instances WHERE directory = ? LIMIT 1", (cwd,)).fetchone()
                    if not dir_match:
                        conn.close()
                        continue

                conn.close()
                archive_info["events"] = event_count
                archive_info["instances"] = instance_count
            except Exception:
                archive_info["events"] = None
                archive_info["instances"] = None

            archives.append(archive_info)
            # Early exit if limit reached
            if limit > 0 and len(archives) >= limit:
                break
        except Exception:
            continue

    # Renumber after filtering
    for i, a in enumerate(archives):
        a["index"] = i + 1

    return archives


def _resolve_archive(selector: str, archives: list[dict]) -> dict | None:
    """Resolve archive by index or name prefix.

    Accepts:
        - Index: "1", "2", etc. (1 = most recent)
        - Full name: "session-2025-12-12_183215"
        - Prefix: "2025-12-12_183215"
    """
    # Try as index first
    try:
        idx = int(selector)
        if 1 <= idx <= len(archives):
            return archives[idx - 1]
    except ValueError:
        pass

    # Try as name or prefix match
    for archive in archives:
        if archive["name"] == selector:
            return archive
        if selector in archive["name"]:
            return archive

    return None


def _query_archive_events(archive: dict, sql_filter: str | None, last: int) -> list[dict]:
    """Query events from archive database."""
    import sqlite3
    from ..core.db import DB_FILE

    db_path = Path(archive["path"]) / DB_FILE
    conn = sqlite3.connect(str(db_path))
    conn.row_factory = sqlite3.Row

    query = "SELECT id, timestamp, type, instance, data FROM events"
    if sql_filter:
        # Use events_v view if it exists for convenience columns
        try:
            conn.execute("SELECT 1 FROM events_v LIMIT 1")
            query = f"SELECT id, timestamp, type, instance, data FROM events_v WHERE {sql_filter}"
        except Exception:
            query = f"SELECT id, timestamp, type, instance, data FROM events WHERE {sql_filter}"
    query += " ORDER BY id DESC"
    if last > 0:
        query += f" LIMIT {last}"

    try:
        rows = conn.execute(query).fetchall()
        events = []
        for row in rows:
            events.append(
                {
                    "id": row["id"],
                    "timestamp": row["timestamp"],
                    "type": row["type"],
                    "instance": row["instance"],
                    "data": json.loads(row["data"]) if row["data"] else {},
                }
            )
        conn.close()
        return list(reversed(events))  # Show oldest first
    except sqlite3.Error as e:
        conn.close()
        raise e


def _query_archive_instances(archive: dict, sql_filter: str | None) -> list[dict]:
    """Query instances from archive database."""
    import sqlite3
    from ..core.db import DB_FILE

    db_path = Path(archive["path"]) / DB_FILE
    conn = sqlite3.connect(str(db_path))
    conn.row_factory = sqlite3.Row

    query = "SELECT name, status, directory, transcript_path, session_id FROM instances"
    if sql_filter:
        query += f" WHERE {sql_filter}"
    query += " ORDER BY created_at DESC"

    try:
        rows = conn.execute(query).fetchall()
        instances = [dict(row) for row in rows]
        conn.close()
        return instances
    except sqlite3.Error as e:
        conn.close()
        raise e


def cmd_archive(argv: list[str], *, ctx: CommandContext | None = None) -> int:
    """List/query archives: hcom archive [<N>|<name>] [events|instances] [--sql] [--last] [--here] [--json]

    Usage:
        hcom archive                              List all archives (numbered)
        hcom archive --here                       Filter to current directory
        hcom archive 1                            Events from most recent
        hcom archive 1 --sql "type='message'"     Filtered events
        hcom archive 1 --last 20                  Last 20 events
        hcom archive 1 agents                     Agents from archive
        hcom archive session-2025-12-12_183215    By stable name (prefix match works)

    Note: --name flag is not used by archive command (no identity needed).
    """
    from .utils import validate_flags, parse_name_flag

    # Archive doesn't use identity; direct calls may still pass --name.
    if ctx is None:
        _, argv = parse_name_flag(argv)

    # Validate flags
    if error := validate_flags("archive", argv):
        print(format_error(error), file=sys.stderr)
        return 1

    # Parse boolean flags first (removes from argv)
    json_output, argv = parse_flag_bool(argv, "--json")
    here_filter, argv = parse_flag_bool(argv, "--here")

    # Parse --last flag
    try:
        last_count, argv = parse_last_flag(argv, default=20)
    except CLIError as e:
        print(format_error(str(e)), file=sys.stderr)
        return 1

    # Parse --sql flag
    from .utils import parse_flag_value
    sql_filter = None
    try:
        sql_filter, argv = parse_flag_value(argv, "--sql")
    except CLIError as e:
        print(format_error(str(e)), file=sys.stderr)
        return 1

    # Remaining args are positional
    clean_argv = [a for a in argv if not a.startswith("--")]

    # Get archives list
    archives = _list_archives(here_filter)

    # No selector = list mode
    if not clean_argv:
        if not archives:
            if json_output:
                print(json.dumps({"archives": [], "count": 0}))
            else:
                print("No archives found")
            return 0

        if json_output:
            print(json.dumps({"archives": archives, "count": len(archives)}, indent=2))
            return 0

        # Human-readable list with index
        print("Archives:")
        for archive in archives:
            events = archive.get("events", "?")
            instances = archive.get("instances", "?")
            print(f"  {archive['index']:>2}. {archive['name']}  {events} events  {instances} agents")
        return 0

    # Selector provided - resolve archive
    selector = clean_argv[0]
    resolved_archive = _resolve_archive(selector, archives)
    if not resolved_archive:
        print(format_error(f"Archive not found: {selector}"), file=sys.stderr)
        print("Run 'hcom archive' to list available archives", file=sys.stderr)
        return 1

    # Subcommand: agents
    if len(clean_argv) > 1 and clean_argv[1] == "agents":
        try:
            instances = _query_archive_instances(resolved_archive, sql_filter)
            if json_output:
                print(json.dumps(instances, indent=2))
            else:
                if not instances:
                    print("Archive is empty")
                else:
                    # Table format
                    print(f"{'name':<8} {'status':<8} {'directory':<40} transcript")
                    for inst in instances:
                        name = inst.get("name", "?")[:8]
                        status = inst.get("status", "?")[:8]
                        directory = shorten_path(inst.get("directory", ""))[:40]
                        transcript = shorten_path(inst.get("transcript_path", ""))
                        print(f"{name:<8} {status:<8} {directory:<40} {transcript}")
            return 0
        except (sqlite3.Error, ValueError) as e:
            print(format_error(f"Query failed: {e}"), file=sys.stderr)
            return 1

    # Default: query events
    try:
        events = _query_archive_events(resolved_archive, sql_filter, last_count)
        if json_output:
            print(json.dumps(events, indent=2))
        else:
            if not events:
                print("No events in archive")
            else:
                for event in events:
                    eid = event["id"]
                    ts = event["timestamp"].split("T")[1][:8] if "T" in event["timestamp"] else event["timestamp"]
                    etype = event["type"]
                    inst = event["instance"]
                    data = event["data"]

                    # Format based on event type
                    if etype == "message":
                        text = data.get("text", "")[:60]
                        if len(data.get("text", "")) > 60:
                            text += "..."
                        print(f'#{eid} {ts} {etype:<8} {inst:<8} "{text}"')
                    elif etype == "status":
                        status = data.get("status", "?")
                        ctx = data.get("context", "")
                        print(f"#{eid} {ts} {etype:<8} {inst:<8} {status} {ctx}")
                    elif etype == "life":
                        action = data.get("action", "?")
                        by = data.get("by", "")
                        print(
                            f"#{eid} {ts} {etype:<8} {inst:<8} {action} by:{by}"
                            if by
                            else f"#{eid} {ts} {etype:<8} {inst:<8} {action}"
                        )
                    else:
                        print(f"#{eid} {ts} {etype:<8} {inst:<8}")
        return 0
    except (sqlite3.Error, ValueError) as e:
        print(format_error(f"Query failed: {e}"), file=sys.stderr)
        return 1


def _print_sh_exports(data: dict, shlex) -> None:
    """Print shell exports for instance data."""
    name = data.get("name", "")
    session_id = data.get("session_id", "")
    status = data.get("status", "unknown")
    directory = data.get("directory", "")

    print(f"export HCOM_INSTANCE_NAME={shlex.quote(name)}")
    print(f"export HCOM_SID={shlex.quote(session_id)}")
    print(f"export HCOM_STATUS={shlex.quote(status)}")
    print(f"export HCOM_DIRECTORY={shlex.quote(directory)}")


def _format_time(timestamp: float) -> str:
    """Format timestamp as relative age string."""
    if not timestamp:
        return "never"
    age = time.time() - timestamp
    return format_age(age) + " ago"


def cmd_list(argv: list[str], *, ctx: CommandContext | None = None) -> int:
    """List instances: hcom list [self|<name>] [field] [-v] [--json|--sh] [--name NAME]"""
    import shlex
    from .utils import (
        resolve_identity,
        parse_name_flag,
        append_unread_messages,
        validate_flags,
    )
    from ..core.instances import (
        load_instance_position,
        cleanup_stale_placeholders,
        cleanup_stale_instances,
    )
    from ..core.messages import get_read_receipts
    from ..core.db import get_db
    from ..shared import SENDER

    # Clean up stale launching placeholders (>2min) and stale instances (>1hr)
    cleanup_stale_placeholders()
    cleanup_stale_instances()

    # Validate flags before parsing
    if error := validate_flags("list", argv):
        print(format_error(error), file=sys.stderr)
        return 1

    # --stopped shortcut: delegate to events command (always JSON output)
    if "--stopped" in argv:
        from .events import cmd_events

        remaining = [a for a in argv if a not in ("--stopped", "--all") and not a.startswith("-")]
        show_all = "--all" in argv
        default_limit = 20
        if remaining:
            # hcom list --stopped NAME → events for specific instance
            instance_name = remaining[0]
            print(json.dumps({"meta": {"type": "stopped_events", "instance": instance_name}}))
            return cmd_events(
                ["--sql", f"life_action='stopped' AND instance='{instance_name}'", "--last", "10000"], ctx=ctx
            )
        else:
            # hcom list --stopped → recent stopped (--all removes limit)
            limit = ["--last", "10000"] if show_all else ["--last", str(default_limit)]
            meta = {"meta": {"type": "stopped_events", "limit": None if show_all else default_limit}}
            if not show_all:
                meta["meta"]["hint"] = "use --all to see all stopped events"
            print(json.dumps(meta))
            return cmd_events(["--sql", "life_action='stopped'"] + limit, ctx=ctx)

    # Identity (instance-only): CLI supplies ctx (preferred). Direct calls may still pass --name.
    from_value = ctx.explicit_name if ctx else None
    if ctx is None:
        from_value, argv = parse_name_flag(argv)

    # Parse boolean flags first (removes from argv)
    json_output, argv = parse_flag_bool(argv, "--json")
    verbose_output, argv = parse_flag_bool(argv, "-v")
    verbose_v2, argv = parse_flag_bool(argv, "--verbose")
    verbose_output = verbose_output or verbose_v2
    sh_output, argv = parse_flag_bool(argv, "--sh")

    # Remaining non-flag args are positional
    target_name = None  # 'self' or instance name
    field_name = None  # Optional field to extract
    positionals = [a for a in argv if not a.startswith("-")]

    # Parse positionals: [target] [field]
    if positionals:
        target_name = positionals[0]
        if len(positionals) > 1:
            field_name = positionals[1]

    # Resolve current instance identity
    # Use explicit --name if provided, otherwise auto-detect
    # Status set by _set_hookless_command_status in cli.py for subagent/codex/adhoc
    if ctx and ctx.identity and ctx.identity.kind == "instance":
        sender_identity = ctx.identity
        current_name = sender_identity.name
    elif from_value:
        try:
            sender_identity = resolve_identity(name=from_value)
            current_name = sender_identity.name
        except Exception as e:
            print(format_error(f"Cannot resolve '{from_value}': {e}"), file=sys.stderr)
            return 1
    else:
        try:
            sender_identity = resolve_identity()
            current_name = sender_identity.name
        except HcomError:
            sender_identity = None
            current_name = None

    # Single instance query: hcom list <name|self> [field] [--json|--sh]
    if target_name:
        # 'self' means current instance
        is_self = target_name == "self"
        if is_self and not sender_identity:
            print(
                format_error("Cannot use 'self' without identity. Run 'hcom start' first."),
                file=sys.stderr,
            )
            return 1

        # Build payload
        if is_self:
            # Self payload - may not have instance data yet
            payload: dict[str, Any] = {
                "name": current_name,
                "session_id": sender_identity.session_id if sender_identity else "",
            }
            if current_name and current_name != SENDER:
                current_data = load_instance_position(current_name)
                if current_data:
                    payload["status"] = current_data.get("status", "unknown")
                    payload["transcript_path"] = current_data.get("transcript_path", "")
                    payload["directory"] = current_data.get("directory", "")
                    payload["parent_name"] = current_data.get("parent_name", "")
                    payload["agent_id"] = current_data.get("agent_id", "")
                    payload["tool"] = current_data.get("tool", "claude")
        else:
            # Named instance - must exist (accept tag-name or base name)
            resolved = resolve_display_name(target_name)
            if resolved:
                target_name = resolved
            data = load_instance_position(target_name)
            if not data:
                print(format_error(f"Not found: {target_name}"), file=sys.stderr)
                print("Use 'hcom list' to see active agents.", file=sys.stderr)
                return 1
            payload = {
                "name": target_name,
                "session_id": data.get("session_id", ""),
                "status": data.get("status", "unknown"),
                "directory": data.get("directory", ""),
                "transcript_path": data.get("transcript_path", ""),
                "parent_name": data.get("parent_name", ""),
                "agent_id": data.get("agent_id", ""),
                "tool": data.get("tool", "claude"),
            }

        # Output based on flags
        if field_name:
            # Extract specific field
            value = payload.get(field_name, "")
            # Normalize booleans to 1/0 for shell
            if isinstance(value, bool):
                value = "1" if value else "0"
            print(value if value else "")
        elif sh_output:
            _print_sh_exports(payload, shlex)
        elif json_output:
            print(json.dumps(payload))
        elif is_self:
            # Self without flags = just name
            print(current_name)
        else:
            # Human readable for named instance
            print(f"{target_name}:")
            print(f"  Status: {payload.get('status', 'unknown')}")
            print(f"  Directory: {payload['directory']}")
            if payload["session_id"]:
                print(f"  Session: {payload['session_id']}")
        return 0

    # Load read receipts for all contexts (bigboss, instances)
    # JSON output gets all receipts; verbose gets 3; default gets 1
    read_limit = None if json_output else (3 if verbose_output else 1)
    read_receipts = get_read_receipts(sender_identity, limit=read_limit) if sender_identity else []

    # Get current instance data for display
    display_data: Any = None
    if current_name and current_name != SENDER:
        display_data = load_instance_position(current_name)

    # Query instances (row exists = active)
    db = get_db()
    rows = db.execute("SELECT * FROM instances ORDER BY created_at DESC").fetchall()

    # Convert rows to dictionaries
    sorted_instances = [dict(row) for row in rows]

    # Calculate unread message counts for LOCAL instances only
    # Remote instances have unknown read positions (last_event_id=0 after import)
    from ..core.messages import get_unread_counts_batch
    from ..core.instances import is_remote_instance

    instances_for_unread = {d["name"]: d for d in sorted_instances if not is_remote_instance(d)}
    unread_counts = get_unread_counts_batch(instances_for_unread)

    if json_output:
        # JSON per line - _self entry first (skip if no identity)
        if sender_identity:
            self_payload = {"_self": {"name": current_name, "read_receipts": read_receipts}}
            if verbose_output and sender_identity.session_id:
                self_payload["_self"]["session_id"] = sender_identity.session_id
            print(json.dumps(self_payload))

        from ..core.db import get_instance_bindings

        for data in sorted_instances:
            # Use full display name ({tag}-{name} or {name})
            full_name = get_full_name(data)
            status, age_str, description, age_seconds, _ = get_instance_status(data)

            # Get binding status (integration tier)
            bindings = get_instance_bindings(data["name"])

            # Parse launch_context JSON
            launch_context = {}
            if data.get("launch_context"):
                try:
                    launch_context = json.loads(data["launch_context"])
                except (json.JSONDecodeError, TypeError):
                    pass

            payload: dict[str, Any] = {  # type: ignore[no-redef]
                "name": full_name,
                "status": status,
                "status_context": data.get("status_context", ""),
                "status_detail": data.get("status_detail", ""),
                "status_age_seconds": int(age_seconds),
                "description": description,
                "unread_count": unread_counts.get(data["name"], 0),
                "headless": bool(data.get("background", False)),
                "session_id": data.get("session_id", ""),
                "directory": data.get("directory", ""),
                "parent_name": data.get("parent_name") or None,
                "agent_id": data.get("agent_id") or None,
                "background_log_file": data.get("background_log_file") or None,
                "transcript_path": data.get("transcript_path") or None,
                "created_at": data.get("created_at"),
                "tag": data.get("tag") or None,
                "tool": data.get("tool", "claude"),
                "base_name": data["name"],
                "hooks_bound": bindings["hooks_bound"],
                "process_bound": bindings["process_bound"],
                "launch_context": launch_context,
            }
            print(json.dumps(payload))
    else:
        # Human-readable - show header with name and read receipts
        # Use full display name (with tag prefix if set)
        display_name = get_full_name(display_data) if display_data else current_name
        if display_name:
            print(f"Your name: {display_name}")
        else:
            print("Your name: (not participating)")

        # Show read receipts if any
        if read_receipts:
            print("  Read receipts:")
            for msg in read_receipts:
                read_count = len(msg["read_by"])
                total = msg["total_recipients"]

                if verbose_output:
                    # Verbose: show list of who has read + ratio
                    readers = ", ".join(msg["read_by"]) if msg["read_by"] else "(none)"
                    print(f'    #{msg["id"]} {msg["age"]} "{msg["text"]}" | read by ({read_count}/{total}): {readers}')
                else:
                    # Default: just show ratio
                    print(f'    #{msg["id"]} {msg["age"]} "{msg["text"]}" | read by {read_count}/{total}')

        print()

        # Check if multiple tool types exist (show tool prefix if so)
        tool_types = set(data.get("tool", "claude") for data in sorted_instances)
        show_tool = len(tool_types) > 1

        # Check if multiple directories exist (show project tag if so)
        from ..shared import get_project_tag

        directories = set(data.get("directory", "") for data in sorted_instances if data.get("directory"))
        show_project = len(directories) > 1

        # Check if multiple terminal types exist (show terminal badge only when different)
        terminal_types: set[str] = set()
        for d in sorted_instances:
            lc_str = d.get("launch_context", "")
            if lc_str:
                try:
                    lc = json.loads(lc_str) if isinstance(lc_str, str) else lc_str
                    preset = lc.get("terminal_preset", "")
                    for pfx in ("tmux", "kitty", "wezterm"):
                        if preset.startswith(pfx):
                            terminal_types.add(pfx)
                            break
                except (json.JSONDecodeError, TypeError):
                    pass
        show_terminal = len(terminal_types) > 1

        # Compute name column width from actual names
        from ..core.instances import is_remote_instance

        max_name_len = 0
        for d in sorted_instances:
            n = len(get_full_name(d))
            if d.get("background", False):
                n += 11  # " [headless]"
            if is_remote_instance(d):
                n += 9  # " [remote]"
            uc = unread_counts.get(d["name"], 0)
            if uc > 0:
                n += len(f" +{uc}")
            max_name_len = max(max_name_len, n)
        name_col_width = max(max_name_len + 2, 14)

        for data in sorted_instances:
            # Use full display name ({tag}-{name} or {name})
            name = get_full_name(data)
            status, age_str, description, age_seconds, _ = get_instance_status(data)
            icon = get_status_icon(data, status)
            # "now" special case (listening status uses age=0)
            age_display = age_str if age_str == "now" else (f"{age_str} ago" if age_str else "")
            # Append "since X" for listening agents idle >= 1 minute
            if status == ST_LISTENING and description == "listening":
                from ..shared import format_listening_since

                description += format_listening_since(data.get("status_time", 0))
            desc_sep = ": " if description else ""

            # Tool prefix (only when multiple tool types exist)
            # Display based on binding: UPPER=pty+hooks, lower=hooks, UPPER*=pty only, lower*=none
            tool_prefix = ""
            if show_tool:
                tool = data.get("tool", "claude")
                from ..core.db import get_instance_bindings

                bindings = get_instance_bindings(data["name"])
                if bindings["process_bound"] and bindings["hooks_bound"]:
                    tool_display = tool.upper()  # CLAUDE - pty + hooks
                elif bindings["process_bound"]:
                    tool_display = tool.upper() + "*"  # CLAUDE* - pty only (unusual)
                elif bindings["hooks_bound"]:
                    tool_display = tool.lower()  # claude - hooks only
                elif tool != "adhoc":
                    tool_display = tool.lower() + "*"  # claude* - no binding
                else:
                    tool_display = "ad-hoc"  # ad-hoc tool type
                tool_prefix = f"[{tool_display}]".ljust(10)

            # Core badges (stay with name — these are important identity info)
            headless_badge = " [headless]" if data.get("background", False) else ""
            remote_badge = " [remote]" if is_remote_instance(data) else ""

            # Unread message indicator
            unread = unread_counts.get(data["name"], 0)
            unread_str = f" +{unread}" if unread > 0 else ""

            # Timeout marker for subagents about to expire
            timeout_marker = ""
            is_subagent = bool(data.get("parent_session_id"))
            if status == ST_LISTENING and is_subagent:
                from ..core.config import get_config
                from ..core.instances import load_instance_position

                parent_name = data.get("parent_name")
                timeout: int | None = None
                if parent_name:
                    parent_data = load_instance_position(parent_name)
                    if parent_data:
                        timeout_val = parent_data.get("subagent_timeout")
                        if isinstance(timeout_val, int):
                            timeout = timeout_val
                if timeout is None:
                    timeout = get_config().subagent_timeout
                time_remaining = timeout - age_seconds
                if 0 < time_remaining < 10:
                    timeout_marker = f" ⏱ {int(time_remaining)}s"

            # Metadata suffix (terminal + project — only when they differ across agents)
            meta_parts: list[str] = []
            if show_terminal:
                lc_str = data.get("launch_context", "")
                if lc_str:
                    try:
                        lc = json.loads(lc_str) if isinstance(lc_str, str) else lc_str
                        preset = lc.get("terminal_preset", "")
                        for pfx in ("tmux", "kitty", "wezterm"):
                            if preset.startswith(pfx):
                                meta_parts.append(f"[{pfx}]")
                                break
                    except (json.JSONDecodeError, TypeError):
                        pass
            if show_project:
                project_tag = get_project_tag(data.get("directory", ""))
                if project_tag:
                    meta_parts.append(project_tag)
            meta_suffix = ("  " + " · ".join(meta_parts)) if meta_parts else ""

            # Build line: {tool}{icon} {name+badges}  {status}{meta}
            name_part = f"{name}{headless_badge}{remote_badge}{unread_str}"
            status_text = f"{age_display}{desc_sep}{description}{timeout_marker}"

            # Main status line
            print(f"{tool_prefix}{icon} {name_part:<{name_col_width}}{status_text}{meta_suffix}")

            if verbose_output:
                # Multi-line detailed view
                from ..core.instances import is_remote_instance
                from ..core.db import get_instance_bindings

                if is_remote_instance(data):
                    # Remote instance: show device info plus available details
                    origin_device = data.get("origin_device_id", "")
                    device_short = origin_device[:8] if origin_device else "(unknown)"

                    # Get device sync time from kv store
                    from ..core.db import kv_get

                    sync_time = 0.0
                    try:
                        ts = kv_get(f"relay_sync_time_{origin_device}")
                        if ts:
                            sync_time = float(ts)
                    except Exception:
                        pass

                    sync_age = _format_time(sync_time) if sync_time else "never"

                    print(f"    device:       {device_short}")
                    print(f"    last_sync:    {sync_age}")

                    session_id = data.get("session_id", "(none)")
                    tool = data.get("tool", "claude")
                    tool_display = "ad-hoc" if tool == "adhoc" else tool
                    print(f"    session_id:   {session_id}")
                    print(f"    tool:         {tool_display}")

                    parent = data.get("parent_name")
                    if parent:
                        print(f"    parent:       {parent}")

                    directory = data.get("directory")
                    if directory:
                        print(f"    directory:    {shorten_path(directory)}")

                    status_time = data.get("status_time", 0)
                    if status_time:
                        print(f"    status_time:  {_format_time(status_time)}")

                    status_detail = data.get("status_detail", "")
                    if status_detail:
                        # Truncate long details
                        max_len = 60
                        detail_display = (
                            status_detail[:max_len] + "..." if len(status_detail) > max_len else status_detail
                        )
                        print(f"    detail:       {detail_display}")

                    print()
                else:
                    # Local instance: show full details
                    session_id = data.get("session_id", "(none)")
                    directory = data.get("directory", "(none)")

                    parent = data.get("parent_name") or "(none)"

                    # Format paths (shorten with ~)
                    log_file = shorten_path(data.get("background_log_file") or "") or "(none)"
                    transcript = shorten_path(data.get("transcript_path") or "") or "(none)"

                    # Format created_at timestamp
                    created_ts = data.get("created_at")
                    created = f"{format_age(time.time() - created_ts)} ago" if created_ts else "(unknown)"

                    # Get subagent agentId if this is a subagent
                    agent_id = None
                    if parent != "(none)":
                        agent_id = data.get("agent_id") or "(none)"

                    # Tool type
                    tool = data.get("tool", "claude")
                    tool_display = "ad-hoc" if tool == "adhoc" else tool

                    # Print indented details
                    print(f"    session_id:   {session_id}")
                    print(f"    tool:         {tool_display}")
                    print(f"    created:      {created}")
                    print(f"    directory:    {directory}")

                    if parent != "(none)":
                        print(f"    parent:       {parent}")
                        print(f"    agent_id:     {agent_id}")

                    # Show binding status (integration tier): pty/hooks/none
                    from ..core.db import format_binding_status

                    bindings = get_instance_bindings(data["name"])
                    bind_str = format_binding_status(bindings)
                    print(f"    bindings:     {bind_str}")

                    # Terminal preset
                    lc_str = data.get("launch_context", "")
                    if lc_str:
                        try:
                            lc = json.loads(lc_str) if isinstance(lc_str, str) else lc_str
                            preset = lc.get("terminal_preset", "")
                            if preset:
                                print(f"    terminal:     {preset}")
                        except (json.JSONDecodeError, TypeError):
                            pass

                    if log_file != "(none)":
                        print(f"    headless log: {log_file}")
                    print(f"    transcript:   {transcript}")

                    status_detail = data.get("status_detail", "")
                    if status_detail:
                        # Truncate long details
                        max_len = 60
                        detail_display = (
                            status_detail[:max_len] + "..." if len(status_detail) > max_len else status_detail
                        )
                        print(f"    detail:       {detail_display}")

                    print()  # Blank line between instances

        # Show orphan processes (running hcom processes not in active instances)
        from ..core.pidtrack import get_orphan_processes

        active_pids: set[int] = {data["pid"] for data in sorted_instances if data.get("pid")}
        orphans = get_orphan_processes(active_pids=active_pids)
        if orphans:
            print("\nOrphan processes:")
            for o in orphans:
                names = o.get("names", [])
                name_str = ", ".join(names) if names else f"pid:{o['pid']}"
                age = format_age(time.time() - o["launched_at"]) if o.get("launched_at") else ""
                age_part = f" · {age}" if age else ""
                print(f"  ◌ {name_str} · {o['tool']}{age_part}")
            if len(orphans) == 1 and orphans[0].get("names"):
                print(f"  → hcom kill {orphans[0]['names'][0]}")
            else:
                print("  → hcom kill <name>")

        # Show recently stopped summary (last 10 minutes)
        from ..core.db import get_recently_stopped, RECENTLY_STOPPED_MINUTES

        active_names = {data["name"] for data in sorted_instances}
        recently_stopped = get_recently_stopped(exclude_active=active_names)
        if recently_stopped:
            names = ", ".join(recently_stopped[:5])
            if len(recently_stopped) > 5:
                names += f" +{len(recently_stopped) - 5}"
            print(f"\nRecently stopped ({RECENTLY_STOPPED_MINUTES}m): {names}")
            print("  → hcom list --stopped [name]")

        # If no instances at all, hint about archives
        total_instances = db.execute("SELECT COUNT(*) FROM instances").fetchone()[0]
        if total_instances == 0:
            archive_dir = hcom_path(ARCHIVE_DIR)
            if archive_dir.exists():
                archive_count = len(list(archive_dir.glob("session-*")))
                if archive_count > 0:
                    print(f"({archive_count} archived session{'s' if archive_count != 1 else ''} - run: hcom archive)")

    # For adhoc instances (--name with instance), append unread messages
    if from_value and sender_identity and sender_identity.kind == "instance" and current_name:
        append_unread_messages(current_name, json_output=json_output)

    # Show one-time tip for first list command
    if current_name:
        from ..core.tips import maybe_show_tip

        maybe_show_tip(current_name, "list:types", json_output=json_output)

    return 0


def cmd_status(argv: list[str], *, ctx: CommandContext | None = None) -> int:
    """Show hcom installation status and diagnostics.

    Usage:
        hcom status          Show hcom directory and hook info
        hcom status --logs   Include recent errors and warnings
        hcom status --json   Machine-readable output
    """
    from ..core.paths import hcom_path, get_project_root, is_hcom_dir_override
    from ..core.db import get_db, init_db
    from ..core.config import get_config
    from ..tools.claude.settings import verify_claude_hooks_installed, get_claude_settings_path
    from ..tools.gemini.settings import (
        verify_gemini_hooks_installed,
        get_gemini_settings_path,
    )
    from ..tools.codex.settings import (
        verify_codex_hooks_installed,
        get_codex_config_path,
    )
    from ..relay import get_relay_status
    from ..terminal import get_available_presets
    from ..shared import get_status_counts
    from ..core.log import get_log_summary, get_recent_logs, get_log_path

    json_output, argv = parse_flag_bool(argv, "--json")
    show_logs, argv = parse_flag_bool(argv, "--logs")

    # Gather status info
    hcom_dir = hcom_path()
    hcom_dir_override = is_hcom_dir_override()
    project_root = get_project_root()

    # Config validation
    config_errors: list[str] = []
    try:
        config = get_config()
    except Exception as e:
        # Parse config error message to extract individual errors
        err_str = str(e)
        if "Invalid config:" in err_str:
            for line in err_str.split("\n"):
                line = line.strip()
                if line.startswith("- "):
                    config_errors.append(line[2:])
        else:
            config_errors.append(err_str)
        # Use defaults for display
        from ..core.config import HcomConfig

        config = HcomConfig()

    # Hook installation status
    claude_hooks = verify_claude_hooks_installed()
    gemini_hooks = verify_gemini_hooks_installed()
    codex_hooks = verify_codex_hooks_installed()

    # CLI tool installation (PATH + fallbacks for claude)
    from ..core.tool_utils import is_tool_installed

    claude_installed = is_tool_installed("claude")
    gemini_installed = is_tool_installed("gemini")
    codex_installed = is_tool_installed("codex")

    # Terminal configuration
    terminal_config = config.terminal
    terminal_available = True
    if terminal_config not in ("default", "custom", "print"):
        # Check if configured preset is available
        presets = dict(get_available_presets())
        terminal_available = presets.get(terminal_config, False)
    # For custom commands, assume available (can't easily verify)

    # Check if hcom directory exists and is writable
    hcom_exists = hcom_dir.exists()
    hcom_writable = False
    if hcom_exists:
        try:
            test_file = hcom_dir / ".write_test"
            test_file.touch()
            test_file.unlink()
            hcom_writable = True
        except (OSError, PermissionError):
            hcom_writable = False

    # Settings paths
    claude_path = get_claude_settings_path()
    gemini_path = get_gemini_settings_path()
    codex_path = get_codex_config_path()

    # Instance status counts
    instance_counts = {
        ST_ACTIVE: 0,
        ST_LISTENING: 0,
        ST_BLOCKED: 0,
        ST_INACTIVE: 0,
        "total": 0,
    }
    try:
        init_db()
        db = get_db()
        rows = db.execute("SELECT * FROM instances").fetchall()
        instances = {row["name"]: dict(row) for row in rows}
        instance_counts = get_status_counts(instances)
        instance_counts["total"] = len(instances)
    except Exception:
        pass  # DB not initialized

    # Relay status
    relay_info = get_relay_status()

    # Log summary
    log_summary = get_log_summary(hours=1.0)
    log_entries: list[dict] = []
    if show_logs:
        log_entries = get_recent_logs(hours=1.0, levels=("ERROR", "WARN"), limit=20)

    # Daemon status with stats if showing logs
    from ..daemon import get_daemon_info, get_delivery_stats
    daemon_info = get_daemon_info(include_stats=show_logs)
    delivery_info = get_delivery_stats(hours=1.0) if show_logs else {}

    # Version/update status
    from ..cli import get_update_info
    from ..shared import __version__

    latest_version, update_cmd = get_update_info()
    version_info = {
        "current": __version__,
        "latest": latest_version,
        "update_available": latest_version is not None,
        "update_cmd": update_cmd,
    }

    status_data = {
        "version": version_info,
        "hcom_dir": str(hcom_dir),
        "hcom_dir_override": hcom_dir_override,
        "hcom_exists": hcom_exists,
        "hcom_writable": hcom_writable,
        "project_root": str(project_root),
        "config_valid": len(config_errors) == 0,
        "config_errors": config_errors,
        "tools": {
            "claude": {
                "installed": claude_installed,
                "hooks": claude_hooks,
                "settings_path": str(claude_path),
            },
            "gemini": {
                "installed": gemini_installed,
                "hooks": gemini_hooks,
                "settings_path": str(gemini_path),
            },
            "codex": {
                "installed": codex_installed,
                "hooks": codex_hooks,
                "settings_path": str(codex_path),
            },
        },
        "terminal": {"config": terminal_config, "available": terminal_available},
        "instances": instance_counts,
        "relay": relay_info,
        "daemon": daemon_info,
        "delivery": delivery_info if show_logs else {},
        "logs": {
            "error_count": log_summary["error_count"],
            "warn_count": log_summary["warn_count"],
            "last_error": log_summary["last_error"],
            "entries": log_entries if show_logs else [],
        },
    }

    if json_output:
        print(json.dumps(status_data, indent=2))
    else:
        if latest_version:
            print(f"hcom {__version__} (update available: v{latest_version})")
            print(f"           {update_cmd}")
        else:
            print(f"hcom {__version__}")
        print()

        # Directory
        dir_status = "ok" if (hcom_exists and hcom_writable) else ("exists" if hcom_exists else "missing")
        print(f"dir:       {hcom_dir} ({dir_status})")
        if hcom_dir_override:
            print(f"           HCOM_DIR={os.environ.get('HCOM_DIR')}")

        # Config
        if config_errors:
            print("config:    ✗ invalid")
            for err in config_errors:
                print(f"           {err}")
        else:
            print("config:    ✓ valid")

        # Tools
        def tool_status(installed: bool, hooks: bool) -> str:
            if installed and hooks:
                return "✓"
            elif installed:
                return "~"
            else:
                return "✗"

        claude_sym = tool_status(claude_installed, claude_hooks)
        gemini_sym = tool_status(gemini_installed, gemini_hooks)
        codex_sym = tool_status(codex_installed, codex_hooks)
        print(f"tools:     Claude {claude_sym}  Gemini {gemini_sym}  Codex {codex_sym}")

        # Terminal
        if terminal_config in ("default", "custom", "print") or "{script}" in terminal_config:
            print(f"terminal:  {terminal_config}")
        else:
            term_status = "✓" if terminal_available else "✗"
            print(f"terminal:  {terminal_config} {term_status}")

        print()

        # Instances
        active = instance_counts.get(ST_ACTIVE, 0)
        listening = instance_counts.get(ST_LISTENING, 0)
        blocked = instance_counts.get(ST_BLOCKED, 0)
        inactive = instance_counts.get(ST_INACTIVE, 0)
        parts = []
        if active:
            parts.append(f"{active} active")
        if listening:
            parts.append(f"{listening} listening")
        if blocked:
            parts.append(f"{blocked} blocked")
        if inactive:
            parts.append(f"{inactive} inactive")
        instance_str = ", ".join(parts) if parts else "none"
        print(f"agents:    {instance_str}")

        # Relay
        if relay_info.get("configured"):
            if relay_info.get("enabled"):
                relay_status = relay_info.get("status", "unknown")
                if relay_status == "ok":
                    print("relay:     connected")
                elif relay_status == "error":
                    error = relay_info.get("error", "unknown error")
                    print(f"relay:     error ({error})")
                else:
                    print("relay:     enabled (not synced)")
            else:
                print("relay:     disabled")
        else:
            print("relay:     not configured")

        # Daemon
        if daemon_info["running"]:
            if daemon_info["responsive"]:
                print(f"daemon:    running (PID {daemon_info['pid']})")
            else:
                print(f"daemon:    running but unresponsive (PID {daemon_info['pid']})")
            # Show stats if available
            if show_logs and "stats" in daemon_info:
                stats = daemon_info["stats"]
                if stats["request_count"] > 0:
                    print(f"           {stats['request_count']} req/hr, avg {stats['avg_ms']}ms, max {stats['max_ms']}ms")
        else:
            print("daemon:    not running")

        # Delivery (only with --logs)
        if show_logs and delivery_info:
            delivered = delivery_info.get("delivered", 0)
            failed = delivery_info.get("failed", 0)
            if delivered > 0 or failed > 0:
                if failed > 0:
                    reasons = delivery_info.get("failure_reasons", {})
                    reason_str = ", ".join(f"{k}:{v}" for k, v in reasons.items())
                    print(f"delivery:  {delivered} ok, {failed} failed ({reason_str})")
                else:
                    print(f"delivery:  {delivered} ok")

        # Logs
        err_count = log_summary["error_count"]
        warn_count = log_summary["warn_count"]
        if err_count == 0 and warn_count == 0:
            print("logs:      ✓ ok")
        else:
            parts = []
            if err_count:
                parts.append(f"{err_count} error{'s' if err_count != 1 else ''}")
            if warn_count:
                parts.append(f"{warn_count} warn{'s' if warn_count != 1 else ''}")
            print(f"logs:      {', '.join(parts)} (1h)")

        # Show log file path and entries if --logs flag
        if show_logs:
            log_path = get_log_path()
            print(f"           {shorten_path(str(log_path))}")
            if log_entries:
                for entry in log_entries:
                    level = entry.get("level", "?")
                    subsystem = entry.get("subsystem", "")
                    event = entry.get("event", "")
                    instance = entry.get("instance", "")
                    ts_str = entry.get("ts", "")

                    # Format timestamp as HH:MM:SS
                    time_display = ts_str[11:19] if len(ts_str) >= 19 else ts_str

                    level_display = "ERROR" if level == "ERROR" else "WARN "
                    instance_part = f" ({instance})" if instance else ""
                    print(f"           {time_display} {level_display} {subsystem}.{event}{instance_part}")

    return 0
