"""Bundle commands for HCOM."""

from __future__ import annotations

import json
import sys
from datetime import datetime, timezone
from typing import Any

from .utils import format_error, validate_flags
from ..shared import CommandContext, format_age, parse_iso_timestamp


def _parse_csv_list(raw: str) -> list[str]:
    if raw is None:
        return []
    parts = [p.strip() for p in raw.split(",")]
    return [p for p in parts if p]


def _get_bundle_by_id(conn, bundle_id_or_prefix: str) -> dict[str, Any] | None:
    """Fetch a bundle by bundle_id prefix or exact event id."""
    if bundle_id_or_prefix.isdigit():
        row = conn.execute(
            "SELECT id, timestamp, data FROM events WHERE type = 'bundle' AND id = ?",
            (int(bundle_id_or_prefix),),
        ).fetchone()
        return dict(row) if row else None

    row = conn.execute(
        """
        SELECT id, timestamp, data
        FROM events
        WHERE type = 'bundle'
          AND json_extract(data, '$.bundle_id') LIKE ?
        ORDER BY id DESC LIMIT 1
        """,
        (f"{bundle_id_or_prefix}%",),
    ).fetchone()
    return dict(row) if row else None


def cmd_bundle(argv: list[str], *, ctx: CommandContext | None = None) -> int:
    """Manage bundles: hcom bundle [list|show|create|chain]"""
    from ..core.db import get_db, init_db
    from ..core.bundles import create_bundle_event, validate_bundle
    from ..core.identity import resolve_identity

    init_db()

    # Default subcommand: list
    argv = argv.copy()
    subcmd = "list"
    if argv and not argv[0].startswith("-"):
        subcmd = argv.pop(0)

    if subcmd not in {"list", "show", "create", "chain"}:
        print(format_error(f"Unknown bundle subcommand: {subcmd}"), file=sys.stderr)
        return 1

    # Validate flags
    if error := validate_flags(f"bundle {subcmd}", argv):
        print(format_error(error), file=sys.stderr)
        return 1

    # Common flags
    json_out = False
    if "--json" in argv:
        argv = [a for a in argv if a != "--json"]
        json_out = True

    conn = get_db()

    if subcmd == "list":
        last = 20
        if "--last" in argv:
            idx = argv.index("--last")
            if idx + 1 >= len(argv) or argv[idx + 1].startswith("-"):
                print(format_error("--last requires a value"), file=sys.stderr)
                return 1
            try:
                last = int(argv[idx + 1])
            except ValueError:
                print(format_error("--last must be an integer"), file=sys.stderr)
                return 1
            argv = argv[:idx] + argv[idx + 2 :]

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
            print("No bundles found")
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
            print(f"{prefix}{b.get('bundle_id', '')} \"{b.get('title', '')}\"")
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

    if "--bundle-file" in argv:
        if bundle is not None:
            print(
                format_error("--bundle and --bundle-file are mutually exclusive"),
                file=sys.stderr,
            )
            return 1
        idx = argv.index("--bundle-file")
        if idx + 1 >= len(argv) or argv[idx + 1].startswith("-"):
            print(format_error("--bundle-file requires a path"), file=sys.stderr)
            return 1
        path = argv[idx + 1]
        try:
            with open(path, "r", encoding="utf-8") as f:
                raw = f.read()
            bundle = json.loads(raw)
        except OSError as e:
            print(format_error(f"Failed to read --bundle-file: {e}"), file=sys.stderr)
            return 1
        except json.JSONDecodeError as e:
            print(format_error(f"Invalid --bundle-file JSON: {e}"), file=sys.stderr)
            return 1
        argv = argv[:idx] + argv[idx + 2 :]

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
            print(format_error("--description is required"), file=sys.stderr)
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
                print(format_error("--events requires a value"), file=sys.stderr)
                return 1
            events = _parse_csv_list(argv[idx + 1])
            argv = argv[:idx] + argv[idx + 2 :]
        if "--files" in argv:
            idx = argv.index("--files")
            if idx + 1 >= len(argv) or argv[idx + 1].startswith("-"):
                print(format_error("--files requires a value"), file=sys.stderr)
                return 1
            files = _parse_csv_list(argv[idx + 1])
            argv = argv[:idx] + argv[idx + 2 :]
        if "--transcript" in argv:
            idx = argv.index("--transcript")
            if idx + 1 >= len(argv) or argv[idx + 1].startswith("-"):
                print(format_error("--transcript requires a value"), file=sys.stderr)
                return 1
            transcript = _parse_csv_list(argv[idx + 1])
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

    if identity.kind == "external":
        bundle_instance = f"ext_{identity.name}"
    elif identity.kind == "system":
        bundle_instance = f"sys_{identity.name}"
    else:
        bundle_instance = identity.name

    bundle_id = create_bundle_event(
        bundle, instance=bundle_instance, created_by=identity.name
    )
    result = {"bundle_id": bundle_id}
    if json_out:
        print(json.dumps(result))
        return 0
    print(bundle_id)
    return 0


__all__ = ["cmd_bundle"]
