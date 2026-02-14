"""Transcript search functionality."""

from __future__ import annotations

import json
import shutil
import sqlite3
import subprocess
from pathlib import Path
from typing import Any

from .paths import get_claude_config_dir


def _get_live_transcript_paths(agent_filter: str | None = None) -> list[str]:
    """Get transcript paths for currently alive agents."""
    from ..db import get_db, init_db

    init_db()
    paths = []
    try:
        db = get_db()
        rows = db.execute(
            "SELECT transcript_path, tool FROM instances WHERE transcript_path IS NOT NULL AND transcript_path != ''"
        ).fetchall()
        for row in rows:
            path = row["transcript_path"]
            tool = row["tool"].lower() if row["tool"] else ""
            if agent_filter:
                if agent_filter == "claude" and "claude" not in tool:
                    continue
                if agent_filter == "gemini" and "gemini" not in tool:
                    continue
                if agent_filter == "codex" and "codex" not in tool:
                    continue
            if path and Path(path).exists():
                paths.append(path)
    except Exception:
        pass
    return paths


def _agent_from_path(path: str) -> str:
    """Determine agent type from transcript path."""
    p = Path(path)

    # Check if path is under Claude config dir (respects CLAUDE_CONFIG_DIR)
    try:
        claude_projects = get_claude_config_dir() / "projects"
        if p.is_relative_to(claude_projects):
            return "claude"
    except (ValueError, AttributeError):
        pass

    # Fallback to checking path parts for other agents
    parts = p.parts
    for i, part in enumerate(parts):
        if part == ".gemini":
            return "gemini"
        if part == ".codex" and i + 1 < len(parts) and parts[i + 1] == "sessions":
            return "codex"

    return "unknown"


def _get_hcom_tracked_paths(agent_filter: str | None = None) -> list[str]:
    """Get transcript paths for all hcom-tracked agents (alive + stopped + archived)."""
    from ..db import get_db, init_db, DB_FILE
    from ..paths import hcom_path, ARCHIVE_DIR

    init_db()
    paths_set: set[str] = set()

    def matches_filter(path: str, tool: str | None = None) -> bool:
        if not agent_filter:
            return True
        if tool:
            tool = tool.lower()
            if agent_filter == "claude" and "claude" in tool:
                return True
            if agent_filter == "gemini" and "gemini" in tool:
                return True
            if agent_filter == "codex" and "codex" in tool:
                return True
        agent = _agent_from_path(path)
        return agent == agent_filter

    # 1. Current instances
    try:
        db = get_db()
        rows = db.execute("SELECT transcript_path, tool FROM instances WHERE transcript_path IS NOT NULL").fetchall()
        for row in rows:
            path = row["transcript_path"]
            if path and matches_filter(path, row["tool"]) and Path(path).exists():
                paths_set.add(path)
    except Exception:
        pass

    # 2. Stopped events
    try:
        db = get_db()
        rows = db.execute("""
            SELECT json_extract(data, '$.snapshot.transcript_path') as path,
                   json_extract(data, '$.snapshot.tool') as tool
            FROM events
            WHERE type = 'life'
              AND json_extract(data, '$.action') = 'stopped'
              AND json_extract(data, '$.snapshot.transcript_path') IS NOT NULL
        """).fetchall()
        for row in rows:
            path = row["path"]
            if path and matches_filter(path, row["tool"]) and Path(path).exists():
                paths_set.add(path)
    except Exception:
        pass

    # 3. Archives
    try:
        archive_dir = hcom_path(ARCHIVE_DIR)
        if archive_dir.exists():
            for session_dir in archive_dir.iterdir():
                if not session_dir.is_dir():
                    continue
                db_path = session_dir / DB_FILE
                if not db_path.exists():
                    continue
                conn = None
                try:
                    conn = sqlite3.connect(str(db_path))
                    conn.row_factory = sqlite3.Row
                    rows = conn.execute("""
                        SELECT json_extract(data, '$.snapshot.transcript_path') as path,
                               json_extract(data, '$.snapshot.tool') as tool
                        FROM events
                        WHERE type = 'life'
                          AND json_extract(data, '$.action') = 'stopped'
                          AND json_extract(data, '$.snapshot.transcript_path') IS NOT NULL
                    """).fetchall()
                    for row in rows:
                        path = row["path"]
                        if path and matches_filter(path, row["tool"]) and Path(path).exists():
                            paths_set.add(path)
                except Exception:
                    pass
                finally:
                    if conn:
                        conn.close()
    except Exception:
        pass

    return list(paths_set)


def _correlate_paths_to_hcom(paths: set[str]) -> dict[str, dict]:
    """Look up hcom agent info for transcript paths."""
    from ..db import get_db, init_db, DB_FILE
    from ..paths import hcom_path, ARCHIVE_DIR

    init_db()
    result = {}

    # 1. Check current instances
    try:
        db = get_db()
        for path in paths:
            row = db.execute("SELECT name FROM instances WHERE transcript_path = ?", (path,)).fetchone()
            if row:
                result[path] = {"name": row["name"], "session": "current"}
    except Exception:
        pass

    # 2. Check stopped events
    remaining = paths - set(result.keys())
    if remaining:
        try:
            db = get_db()
            for path in remaining:
                row = db.execute(
                    """
                    SELECT instance FROM events
                    WHERE type = 'life'
                      AND json_extract(data, '$.action') = 'stopped'
                      AND json_extract(data, '$.snapshot.transcript_path') = ?
                    ORDER BY id DESC LIMIT 1
                """,
                    (path,),
                ).fetchone()
                if row:
                    result[path] = {"name": row["instance"], "session": "current"}
        except Exception:
            pass

    # 3. Check archives
    remaining = paths - set(result.keys())
    if remaining:
        try:
            archive_dir = hcom_path(ARCHIVE_DIR)
            if archive_dir.exists():
                for session_dir in sorted(archive_dir.iterdir(), reverse=True):
                    if not session_dir.is_dir():
                        continue
                    db_path = session_dir / DB_FILE
                    if not db_path.exists():
                        continue
                    conn = None
                    try:
                        conn = sqlite3.connect(str(db_path))
                        conn.row_factory = sqlite3.Row
                        for path in list(remaining):
                            row = conn.execute(
                                """
                                SELECT instance FROM events
                                WHERE type = 'life'
                                  AND json_extract(data, '$.action') = 'stopped'
                                  AND json_extract(data, '$.snapshot.transcript_path') = ?
                                ORDER BY id DESC LIMIT 1
                            """,
                                (path,),
                            ).fetchone()
                            if row:
                                result[path] = {"name": row["instance"], "session": session_dir.name}
                                remaining.discard(path)
                    except Exception:
                        pass
                    finally:
                        if conn:
                            conn.close()
                    if not remaining:
                        break
        except Exception:
            pass

    return result


def search_transcripts(
    pattern: str,
    limit: int = 20,
    agent_filter: str | None = None,
    scope: str = "hcom",
    exclude_self: str | None = None,
) -> dict[str, Any]:
    """Search transcripts using two-phase ripgrep/grep.

    Uses a two-phase approach to avoid loading massive transcript lines into memory:
    1. Phase 1: Get list of matching files only (fast, tiny output)
    2. Phase 2: For each file (sorted by mtime, recent first), extract matches
       with truncated line content until we hit the limit

    Args:
        pattern: Search pattern (regex).
        limit: Max results to return.
        agent_filter: 'claude', 'gemini', 'codex', or None.
        scope: 'live' (alive only), 'hcom' (all tracked), or 'all' (all on disk).
        exclude_self: hcom name to exclude from results (filters after correlation).

    Returns:
        Dict with 'results' (list), 'count' (int), 'scope' (str),
        and 'limit_hit' (bool) indicating if results were truncated.
        Each result has: path, line, agent, text, hcom_name (opt), hcom_session (opt).
    """
    # Build search paths
    home = Path.home()
    search_paths: list[str] = []
    is_file_list = False

    if scope == "live":
        search_paths = _get_live_transcript_paths(agent_filter)
        is_file_list = True
        if not search_paths:
            return {"results": [], "count": 0, "scope": scope}
    elif scope == "hcom":
        search_paths = _get_hcom_tracked_paths(agent_filter)
        is_file_list = True
        if not search_paths:
            return {"results": [], "count": 0, "scope": scope}
    else:  # scope == "all"
        if agent_filter is None or agent_filter == "claude":
            p = get_claude_config_dir() / "projects"
            if p.exists():
                search_paths.append(str(p))
        if agent_filter is None or agent_filter == "gemini":
            p = home / ".gemini"
            if p.exists():
                search_paths.append(str(p))
        if agent_filter is None or agent_filter == "codex":
            p = home / ".codex" / "sessions"
            if p.exists():
                search_paths.append(str(p))
        if not search_paths:
            return {"results": [], "count": 0, "scope": scope}

    use_rg = shutil.which("rg") is not None
    results: list[dict[str, Any]] = []

    try:
        # === PHASE 1: Get list of matching files ===
        matching_files: list[str] = []

        if use_rg:
            cmd = ["rg", "-l", pattern]
            if not is_file_list:
                cmd.extend(["--glob", "*.jsonl", "--glob", "*.json"])
            cmd.extend(search_paths)
            proc = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
            if proc.stdout.strip():
                matching_files = proc.stdout.strip().split("\n")
        else:
            if is_file_list:
                cmd = ["grep", "-l", pattern] + search_paths
            else:
                cmd = ["grep", "-rl", "--include=*.jsonl", "--include=*.json", pattern] + search_paths
            proc = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
            if proc.stdout.strip():
                matching_files = proc.stdout.strip().split("\n")

        if not matching_files:
            return {"results": [], "count": 0, "scope": scope}

        # Resolve exclude_self to transcript path for early filtering
        exclude_path: str | None = None
        if exclude_self:
            path_to_hcom = _correlate_paths_to_hcom(set(matching_files))
            for tp, info in path_to_hcom.items():
                if info["name"] == exclude_self:
                    exclude_path = tp
                    break

        # Filter by agent type and get mtimes
        files_with_mtime: list[tuple[str, float]] = []
        for f in matching_files:
            if not f:
                continue
            if exclude_path and f == exclude_path:
                continue
            agent = _agent_from_path(f)
            if agent_filter and agent != agent_filter:
                continue
            try:
                mtime = Path(f).stat().st_mtime
                files_with_mtime.append((f, mtime))
            except OSError:
                continue

        # Sort by mtime descending (most recent first)
        files_with_mtime.sort(key=lambda x: x[1], reverse=True)

        # === PHASE 2: Extract matches from files until we hit limit ===
        for file_path, _ in files_with_mtime:
            if len(results) >= limit:
                break

            remaining = limit - len(results)
            agent = _agent_from_path(file_path)

            if use_rg:
                # Use --max-columns to truncate huge transcript lines
                cmd = ["rg", "--json", "--max-columns", "500", "-m", str(remaining), pattern, file_path]
                proc = subprocess.run(cmd, capture_output=True, text=True, timeout=10)

                for line in proc.stdout.splitlines():
                    if len(results) >= limit:
                        break
                    try:
                        data = json.loads(line)
                        if data.get("type") == "match":
                            match_data = data["data"]
                            line_num = match_data["line_number"]
                            text = match_data["lines"]["text"].strip()
                            results.append(
                                {
                                    "path": file_path,
                                    "line": line_num,
                                    "agent": agent,
                                    "text": text,
                                }
                            )
                    except (json.JSONDecodeError, KeyError):
                        continue
            else:
                cmd = ["grep", "-Hn", "-m", str(remaining), pattern, file_path]
                proc = subprocess.run(cmd, capture_output=True, text=True, timeout=10)

                for line in proc.stdout.splitlines():
                    if len(results) >= limit:
                        break
                    # Parse file:line:content format
                    prefix = file_path + ":"
                    if not line.startswith(prefix):
                        continue
                    rest = line[len(prefix):]
                    colon_idx = rest.find(":")
                    if colon_idx == -1:
                        continue
                    line_num_str = rest[:colon_idx]
                    text = rest[colon_idx + 1:]
                    if not line_num_str.isdigit():
                        continue
                    # Truncate text for consistency with rg --max-columns
                    if len(text) > 500:
                        text = text[:500] + " [truncated]"
                    results.append(
                        {
                            "path": file_path,
                            "line": int(line_num_str),
                            "agent": agent,
                            "text": text,
                        }
                    )

    except subprocess.TimeoutExpired:
        return {"results": results, "count": len(results), "scope": scope, "limit_hit": False, "error": "Search timed out"}
    except Exception as e:
        return {"results": [], "count": 0, "scope": scope, "limit_hit": False, "error": f"Search failed: {e}"}

    limit_hit = len(results) >= limit

    if results:
        unique_paths = set(r["path"] for r in results)
        path_to_hcom = _correlate_paths_to_hcom(unique_paths)
        for r in results:
            if r["path"] in path_to_hcom:
                info = path_to_hcom[r["path"]]
                r["hcom_name"] = info["name"]
                r["hcom_session"] = info["session"]

    return {"results": results, "count": len(results), "scope": scope, "limit_hit": limit_hit}
