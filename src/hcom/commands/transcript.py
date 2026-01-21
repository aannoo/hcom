"""Transcript commands for HCOM"""

import sys
import json
import re
from .utils import format_error
from ..shared import CommandContext


def cmd_transcript(argv: list[str], *, ctx: CommandContext | None = None) -> int:
    """Get conversation transcript: hcom transcript @instance [N | N-M] [--json] [--full] [--detailed] [--name NAME]"""
    from .utils import (
        validate_flags,
        parse_name_flag,
        append_unread_messages,
        resolve_identity,
    )
    from ..core.instances import load_instance_position
    from ..core.transcript import get_thread, format_thread, format_thread_detailed
    from ..core.db import get_db

    # Identity (instance-only): CLI supplies ctx (preferred). Direct calls may still pass --name.
    from_value = ctx.explicit_name if ctx else None
    if ctx is None:
        from_value, argv = parse_name_flag(argv)

    # Check for subcommands
    if argv and argv[0] == "timeline":
        return _cmd_transcript_timeline(argv[1:])
    if argv and argv[0] == "search":
        return _cmd_transcript_search(argv[1:])

    # Validate flags
    if error := validate_flags("transcript", argv):
        print(format_error(error), file=sys.stderr)
        return 1
    instance_name = None
    if ctx and ctx.identity and ctx.identity.kind == "instance":
        instance_name = ctx.identity.name
    elif from_value:
        try:
            identity = resolve_identity(name=from_value)
            if identity.kind == "instance":
                instance_name = identity.name
        except Exception:
            pass  # Not critical - just won't append messages

    def parse_position_or_range(arg: str) -> tuple[int, int] | None:
        """Parse 'N' or 'N-M' into (start, end) tuple, or None if not a position."""
        # Single position: just a number
        if re.match(r"^\d+$", arg):
            pos = int(arg)
            return (pos, pos)
        # Range: N-M
        match = re.match(r"^(\d+)-(\d+)$", arg)
        if match:
            return (int(match.group(1)), int(match.group(2)))
        return None

    # Parse arguments
    target = None
    last = 10
    json_output = False
    full_output = False
    detailed_output = False
    range_tuple = None

    i = 0
    while i < len(argv):
        arg = argv[i]
        if arg == "--json":
            json_output = True
        elif arg == "--full":
            full_output = True
        elif arg == "--detailed":
            detailed_output = True
        elif arg == "--last" and i + 1 < len(argv):
            try:
                last = int(argv[i + 1])
                i += 1
            except ValueError:
                print(format_error("--last requires a number"), file=sys.stderr)
                return 1
        elif arg == "--range" and i + 1 < len(argv):
            parsed = parse_position_or_range(argv[i + 1])
            if not parsed:
                print(
                    format_error("--range requires N or N-M format (e.g. --range 5 or --range 5-10)"),
                    file=sys.stderr,
                )
                return 1
            start, end = parsed
            if start < 1 or end < 1:
                print(format_error("positions must be >= 1"), file=sys.stderr)
                return 1
            if start > end:
                print(format_error("range start must be <= end"), file=sys.stderr)
                return 1
            range_tuple = (start, end)
            i += 1
        elif arg.startswith("@"):
            target = arg[1:]  # Strip @
        elif not arg.startswith("-"):
            # Check if it's a position/range first
            parsed = parse_position_or_range(arg)
            if parsed:
                start, end = parsed
                if start < 1 or end < 1:
                    print(format_error("positions must be >= 1"), file=sys.stderr)
                    return 1
                if start > end:
                    print(format_error("range start must be <= end"), file=sys.stderr)
                    return 1
                range_tuple = (start, end)
            else:
                target = arg
        i += 1

    # Resolve target instance
    if target:
        # Look up by name
        data = load_instance_position(target)
        if not data:
            # Try prefix match
            conn = get_db()
            row = conn.execute("SELECT name FROM instances WHERE name LIKE ? LIMIT 1", (f"{target}%",)).fetchone()
            if row:
                target = row["name"]
                data = load_instance_position(target)

        if not data:
            # Check life event snapshots (stopped instances)
            conn = get_db()
            row = conn.execute(
                """
                SELECT json_extract(data, '$.snapshot.transcript_path') as transcript_path,
                       json_extract(data, '$.snapshot.tool') as tool
                FROM events
                WHERE instance = ? AND type = 'life'
                  AND json_extract(data, '$.action') = 'stopped'
                ORDER BY id DESC LIMIT 1
            """,
                (target,),
            ).fetchone()
            if row and row["transcript_path"]:
                data = {
                    "transcript_path": row["transcript_path"],
                    "tool": row["tool"] or "claude",
                }
            else:
                print(f"Error: '{target}' not found", file=sys.stderr)
                print(
                    "Use 'hcom list' to see available agents, or 'hcom archive' for past sessions.",
                    file=sys.stderr,
                )
                return 1

        transcript_path = data.get("transcript_path", "")
        instance_name = target
    else:
        # No target specified - try to default to self
        try:
            identity = resolve_identity()
            if identity.kind == "instance" and identity.instance_data:
                instance_name = identity.name
                data = identity.instance_data
                transcript_path = data.get("transcript_path", "")
            else:
                raise ValueError("Not an instance")
        except Exception:
            # Cannot resolve self - require @instance
            from .utils import get_command_help

            print(format_error("Target required"), file=sys.stderr)
            print("Usage: hcom transcript @name [N | N-M]\n", file=sys.stderr)
            print(get_command_help("transcript"), file=sys.stderr)
            return 1

    # Get tool type for parser selection
    tool = data.get("tool", "claude") if data else "claude"

    if not transcript_path:
        # Check if this is an adhoc instance (no transcript available)
        if tool == "adhoc":
            print(f"Error: '{instance_name}' is ad-hoc (no transcript)", file=sys.stderr)
            print("Ad-hoc agents don't have conversation transcripts", file=sys.stderr)
        else:
            print(f"Error: No transcript path for '{instance_name}'", file=sys.stderr)
            print("May not have started a conversation yet", file=sys.stderr)
        return 1

    # Get thread using tool-specific parser
    thread_data = get_thread(
        transcript_path,
        last=last,
        tool=tool,
        detailed=detailed_output,
        range_tuple=range_tuple,
    )

    if thread_data.get("error"):
        print(f"Error: {thread_data['error']}", file=sys.stderr)
        return 1

    # Output
    if json_output:
        print(json.dumps(thread_data, indent=2))
    elif detailed_output:
        print(format_thread_detailed(thread_data, instance_name))
    else:
        print(format_thread(thread_data, instance_name, full=full_output))

    # Append unread messages for adhoc instances (--name with instance)
    if from_value and instance_name:
        append_unread_messages(instance_name, json_output=json_output)

    return 0


def _cmd_transcript_timeline(argv: list[str]) -> int:
    """Timeline subcommand: hcom transcript timeline [--last N] [--full] [--detailed] [--json]"""
    from ..core.transcript import (
        get_timeline,
        format_timeline,
        format_timeline_detailed,
    )
    from ..core.db import get_db

    # Parse arguments
    last = 10
    json_output = False
    full_output = False
    detailed_output = False

    i = 0
    while i < len(argv):
        arg = argv[i]
        if arg == "--json":
            json_output = True
        elif arg == "--full":
            full_output = True
        elif arg == "--detailed":
            detailed_output = True
        elif arg == "--last" and i + 1 < len(argv):
            try:
                last = int(argv[i + 1])
                i += 1
            except ValueError:
                print(format_error("--last requires a number"), file=sys.stderr)
                return 1
        elif arg == "--range" or re.match(r"^\d+-\d+$", arg) or re.match(r"^\d+$", arg):
            # Range not supported for timeline
            print(format_error("Range not supported for timeline"), file=sys.stderr)
            print(
                "Use --last N instead, then drill into specific exchanges with:",
                file=sys.stderr,
            )
            print("  hcom transcript @name N", file=sys.stderr)
            return 1
        elif arg.startswith("-"):
            print(f"Error: Unknown flag '{arg}'", file=sys.stderr)
            return 1
        i += 1

    # Get all instances with transcript paths (active + stopped from snapshots)
    conn = get_db()

    # Active instances
    active = conn.execute("""
        SELECT name, transcript_path, tool FROM instances
        WHERE transcript_path IS NOT NULL AND transcript_path != ''
    """).fetchall()

    # Stopped instances from life event snapshots
    stopped = conn.execute("""
        SELECT instance as name,
               json_extract(data, '$.snapshot.transcript_path') as transcript_path,
               json_extract(data, '$.snapshot.tool') as tool
        FROM events
        WHERE type = 'life' AND json_extract(data, '$.action') = 'stopped'
          AND json_extract(data, '$.snapshot.transcript_path') IS NOT NULL
    """).fetchall()

    # Combine, deduping by name (active takes precedence)
    seen = set()
    instances = []
    for row in active:
        seen.add(row["name"])
        instances.append(
            {
                "name": row["name"],
                "transcript_path": row["transcript_path"],
                "tool": row["tool"] or "claude",
            }
        )
    for row in stopped:
        if row["name"] not in seen:
            seen.add(row["name"])
            instances.append(
                {
                    "name": row["name"],
                    "transcript_path": row["transcript_path"],
                    "tool": row["tool"] or "claude",
                }
            )

    if not instances:
        print("No agents with transcripts found", file=sys.stderr)
        return 1

    # Get timeline
    timeline_data = get_timeline(instances, last=last, detailed=detailed_output)

    if timeline_data.get("error"):
        print(f"Error: {timeline_data['error']}", file=sys.stderr)
        return 1

    # Output
    if json_output:
        print(json.dumps(timeline_data, indent=2))
    elif detailed_output:
        print(format_timeline_detailed(timeline_data))
    else:
        print(format_timeline(timeline_data, full=full_output))

    return 0








def _cmd_transcript_search(argv: list[str]) -> int:
    """Search transcripts: hcom transcript search "pattern" [--live] [--all] [--limit N] [--json] [--agent TYPE]

    Scopes:
      (default)  hcom-tracked transcripts only (alive + stopped agents)
      --live     only currently alive agents
      --all      all transcripts on disk (includes non-hcom sessions)

    Uses ripgrep (fast) or grep (fallback).
    """
    from ..core.transcript import search_transcripts

    # Parse arguments
    pattern = None
    limit = 20
    json_output = False
    agent_filter = None  # claude, gemini, codex, or None for all
    scope = "hcom"  # default: hcom-tracked only

    i = 0
    while i < len(argv):
        arg = argv[i]
        if arg == "--json":
            json_output = True
        elif arg == "--live":
            scope = "live"
        elif arg == "--all":
            scope = "all"
        elif arg == "--limit" and i + 1 < len(argv):
            try:
                limit = int(argv[i + 1])
                if limit <= 0:
                    print(format_error("--limit must be positive"), file=sys.stderr)
                    return 1
                i += 1
            except ValueError:
                print(format_error("--limit requires a number"), file=sys.stderr)
                return 1
        elif arg == "--agent" and i + 1 < len(argv):
            agent_filter = argv[i + 1].lower()
            if agent_filter not in ("claude", "gemini", "codex"):
                print(format_error("--agent must be claude, gemini, or codex"), file=sys.stderr)
                return 1
            i += 1
        elif arg.startswith("-"):
            print(format_error(f"Unknown flag: {arg}"), file=sys.stderr)
            return 1
        elif pattern is None:
            pattern = arg
        i += 1

    if not pattern:
        print(format_error("Pattern required"), file=sys.stderr)
        print(
            'Usage: hcom transcript search "pattern" [--live] [--all] [--limit N] [--json] [--agent TYPE]',
            file=sys.stderr,
        )
        return 1

    # Use core search function
    result = search_transcripts(
        pattern, limit=limit, agent_filter=agent_filter, scope=scope
    )

    if result.get("error"):
        print(f"Search error: {result['error']}", file=sys.stderr)
        return 1

    results = result.get("results", [])
    # scope might be updated by search (e.g. valid fallback) but we use input scope for label mostly
    # Actually core search logic returns results directly.

    # Output
    if json_output:
        # Truncate text for JSON output
        for r in results:
            if len(r["text"]) > 200:
                r["text"] = r["text"][:200] + "..."
        print(json.dumps({"count": len(results), "results": results, "scope": scope}, indent=2))
    else:
        if not results:
            print(f"No matches for '{pattern}'")
        else:
            scope_label = {"live": " (live agents)", "hcom": " (hcom-tracked)", "all": ""}[scope]
            print(f"Found {len(results)} matches{scope_label}:\n")
            for r in results:
                # Truncate path for display
                path = r["path"]
                if len(path) > 60:
                    path = "..." + path[-57:]
                # Show hcom agent name if known
                if "hcom_name" in r:
                    agent_str = f"{r['agent']}:{r['hcom_name']}"
                    if r.get("hcom_session") and r["hcom_session"] != "current":
                        agent_str += f"@{r['hcom_session']}"
                else:
                    agent_str = r["agent"]
                print(f"[{agent_str}] {path}:{r['line']}")
                # Show snippet, truncate consistently at display time
                snippet = r["text"].replace("\n", " ")
                if len(snippet) > 100:
                    snippet = snippet[:100] + "..."
                print(f"    {snippet}\n")

    return 0



