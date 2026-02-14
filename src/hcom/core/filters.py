"""Composable filter system for events queries, subscriptions, and listen.

This module provides shared logic for parsing, validating, and generating SQL
from event filter flags. It enables uniform filter syntax across:
- hcom events (queries)
- hcom events sub (subscriptions)
- hcom listen (blocking waits)

Example usage:
    # Parse flags from argv
    filters, remaining = parse_event_flags(['--agent', 'peso', '--status', 'listening'])

    # Validate no conflicting type requirements
    validate_type_constraints(filters)

    # Generate SQL WHERE clause
    sql = build_sql_from_flags(filters)
    # Returns: "instance = 'peso' AND type = 'status' AND status_val = 'listening'"
"""

from typing import Any

from ..shared import ST_LISTENING, ST_BLOCKED


# Mapping of CLI flags to internal filter keys
FLAG_MAP = {
    "--agent": "instance",
    "--type": "type",
    "--status": "status",
    "--context": "context",
    "--file": "file",
    "--cmd": "cmd",
    "--from": "from",
    "--mention": "mention",
    "--action": "action",
    "--after": "after",
    "--before": "before",
    "--intent": "intent",
    "--thread": "thread",
    "--reply-to": "reply_to",
    "--collision": "collision",
}

# Flags that require specific event types
STATUS_FLAGS = {"status", "context", "file", "cmd"}
MESSAGE_FLAGS = {"from", "mention", "intent", "thread", "reply_to"}
LIFE_FLAGS = {"action"}

# Tool context constants for SQL filters
# File-write tool contexts by platform:
#   Claude: tool:Write, tool:Edit
#   Gemini: tool:write_file, tool:replace
#   Codex: tool:apply_patch
FILE_WRITE_CONTEXTS = "('tool:Write', 'tool:Edit', 'tool:write_file', 'tool:replace', 'tool:apply_patch')"


# All file operation contexts (for queries and formatting)
FILE_OP_CONTEXTS = (
    "tool:Write",
    "tool:Edit",
    "tool:Read",
    "tool:write_file",
    "tool:replace",
    "tool:read_file",
    "tool:apply_patch",
)
FILE_OP_CONTEXTS_SQL = f"({','.join(repr(c) for c in FILE_OP_CONTEXTS)})"

# Shell tool contexts:
#   Claude: tool:Bash
#   Gemini: tool:run_shell_command
#   Codex:  tool:shell (via TranscriptWatcher)
SHELL_TOOL_CONTEXTS = "('tool:Bash', 'tool:run_shell_command', 'tool:shell')"


def parse_event_flags(argv: list[str]) -> tuple[dict[str, list[Any]], list[str]]:
    """Parse event filter flags from argv.

    Extracts known filter flags and their values, preserving non-filter args.
    Multiple instances of the same flag are collected into a list (OR semantics).

    Args:
        argv: Command line arguments list

    Returns:
        Tuple of (filters_dict, remaining_argv)
        filters_dict: {filter_key: [value1, value2, ...]}
        remaining_argv: Non-filter arguments

    Example:
        >>> parse_event_flags(['--agent', 'peso', '--last', '20'])
        ({'instance': ['peso']}, ['--last', '20'])
    """
    filters: dict[str, list[Any]] = {}
    remaining: list[str] = []

    i = 0
    while i < len(argv):
        arg = argv[i]

        # Boolean flag (no value)
        if arg == "--collision":
            filters.setdefault("collision", []).append(True)
            i += 1
        # Value flag
        elif arg in FLAG_MAP:
            if i + 1 >= len(argv):
                raise ValueError(f"Flag {arg} requires a value")
            key = FLAG_MAP[arg]
            value = argv[i + 1]
            filters.setdefault(key, []).append(value)
            i += 2
        # Non-filter arg
        else:
            remaining.append(arg)
            i += 1

    # Resolve tag-name to base name for instance filters
    if "instance" in filters:
        from .instances import resolve_display_name

        resolved = []
        for name in filters["instance"]:
            r = resolve_display_name(name)
            resolved.append(r if r else name)
        filters["instance"] = resolved

    return filters, remaining


def validate_type_constraints(filters: dict[str, list[Any]]) -> None:
    """Validate that filters don't mix incompatible event types.

    Some filters require specific event types:
    - status, context, file, cmd → require type='status'
    - from, mention, intent, thread, reply_to → require type='message'
    - action → requires type='life'

    Raises:
        ValueError: If filters require conflicting event types

    Example:
        >>> validate_type_constraints({'cmd': ['git'], 'from': ['bigboss']})
        ValueError: Cannot combine filters from different event types: life, message
    """
    required_types = set()

    # Check which types are required by filters
    if any(flag in filters for flag in STATUS_FLAGS):
        required_types.add("status")
    if any(flag in filters for flag in MESSAGE_FLAGS):
        required_types.add("message")
    if any(flag in filters for flag in LIFE_FLAGS):
        required_types.add("life")

    # Explicit --type flag takes precedence
    if "type" in filters:
        explicit_types = set(filters["type"])
        # Check if explicit type conflicts with inferred requirements
        if required_types and not required_types.issubset(explicit_types):
            conflicting = required_types - explicit_types
            raise ValueError(f"Filters require type {conflicting} but --type specified {explicit_types}")

    # Check for conflicting requirements
    if len(required_types) > 1:
        raise ValueError(
            f"Cannot combine filters from different event types: {', '.join(sorted(required_types))}\n"
            f"Status filters: --status, --context, --file, --cmd\n"
            f"Message filters: --from, --mention, --intent, --thread, --reply-to\n"
            f"Life filters: --action"
        )


def _escape_sql(s: str) -> str:
    """Escape single quotes for SQL string literals.

    Prevents SQL injection by escaping quotes.

    Args:
        s: String to escape

    Returns:
        Escaped string safe for SQL

    Example:
        >>> _escape_sql("O'Reilly")
        "O''Reilly"
    """
    return s.replace("'", "''")


def _escape_sql_like(s: str) -> str:
    """Escape LIKE wildcards and quotes for SQL LIKE patterns.

    Escapes: backslash, %, _, and single quotes
    Uses backslash as escape character (compatible with ESCAPE '\\' in SQLite)

    Args:
        s: String to escape

    Returns:
        Escaped string safe for SQL LIKE with ESCAPE '\\'

    Example:
        >>> _escape_sql_like("50%_off")
        "50\\%\\_off"
    """
    return s.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_").replace("'", "''")


def build_sql_from_flags(filters: dict[str, list[Any]]) -> str:
    """Build SQL WHERE clause from filter flags.

    Composition rules:
    - Multiple values for same flag = OR (any can match)
    - Different flags = AND (all must match)
    - Automatically infers type based on filter flags used

    Pattern matching (uniform across all pattern flags):
    - "git" → contains "git" (LIKE %git%)
    - "^git" → starts with "git" (LIKE git%)
    - "git$" → ends with "git" (LIKE %git)
    - "=git status" → exact match (= "git status")
    - "*.py" → glob (replace * with %, LIKE %.py)

    Args:
        filters: Parsed filter dictionary from parse_event_flags()

    Returns:
        SQL WHERE clause string (without leading WHERE)
        Returns empty string if no filters

    Raises:
        ValueError: If filters have conflicting type requirements

    Example:
        >>> build_sql_from_flags({'instance': ['peso'], 'status': ['listening']})
        "instance = 'peso' AND type = 'status' AND status_val = 'listening'"
    """
    if not filters:
        return ""

    # Validate first - catch type conflicts before generating SQL
    validate_type_constraints(filters)

    clauses: list[str] = []

    # Instance filter
    if "instance" in filters:
        if len(filters["instance"]) == 1:
            clauses.append(f"instance = '{_escape_sql(filters['instance'][0])}'")
        else:
            instances = "', '".join(_escape_sql(x) for x in filters["instance"])
            clauses.append(f"instance IN ('{instances}')")

    # Type filter (explicit)
    if "type" in filters:
        if len(filters["type"]) == 1:
            clauses.append(f"type = '{_escape_sql(filters['type'][0])}'")
        else:
            types = "', '".join(_escape_sql(x) for x in filters["type"])
            clauses.append(f"type IN ('{types}')")
    # Auto-infer type from filters (validation already checked no conflicts)
    elif any(flag in filters for flag in STATUS_FLAGS):
        clauses.append("type = 'status'")
    elif any(flag in filters for flag in MESSAGE_FLAGS):
        clauses.append("type = 'message'")
    elif any(flag in filters for flag in LIFE_FLAGS):
        clauses.append("type = 'life'")

    # Status filter
    if "status" in filters:
        if len(filters["status"]) == 1:
            clauses.append(f"status_val = '{_escape_sql(filters['status'][0])}'")
        else:
            statuses = "', '".join(_escape_sql(x) for x in filters["status"])
            clauses.append(f"status_val IN ('{statuses}')")

    # Context filter
    if "context" in filters:
        context_clauses = []
        for pattern in filters["context"]:
            if "*" in pattern:
                # Glob pattern: tool:* → tool:%
                # Split by *, escape each part, join with % (avoids null byte marker)
                parts = pattern.split("*")
                sql_pattern = "%".join(_escape_sql_like(p) for p in parts)
                context_clauses.append(f"status_context LIKE '{sql_pattern}' ESCAPE '\\'")
            else:
                # Exact match
                context_clauses.append(f"status_context = '{_escape_sql(pattern)}'")

        if len(context_clauses) == 1:
            clauses.append(context_clauses[0])
        else:
            clauses.append(f"({' OR '.join(context_clauses)})")

    # File filter (status_detail for file write tools)
    if "file" in filters:
        clauses.append(f"status_context IN {FILE_WRITE_CONTEXTS}")

        file_clauses = []
        for pattern in filters["file"]:
            if "*" in pattern:
                # Glob: *.py → %.py
                # Split by *, escape each part, join with % (avoids null byte marker)
                parts = pattern.split("*")
                sql_pattern = "%".join(_escape_sql_like(p) for p in parts)
                file_clauses.append(f"status_detail LIKE '{sql_pattern}' ESCAPE '\\'")
            else:
                # Contains (default)
                file_clauses.append(f"status_detail LIKE '%{_escape_sql_like(pattern)}%' ESCAPE '\\'")

        if len(file_clauses) == 1:
            clauses.append(file_clauses[0])
        else:
            clauses.append(f"({' OR '.join(file_clauses)})")

    # Cmd filter (status_detail for shell tools)
    if "cmd" in filters:
        clauses.append(f"status_context IN {SHELL_TOOL_CONTEXTS}")

        cmd_clauses = []
        for pattern in filters["cmd"]:
            if pattern.startswith("="):
                # Exact match: =git status
                cmd_clauses.append(f"status_detail = '{_escape_sql(pattern[1:])}'")
            elif pattern.startswith("^"):
                # Starts with: ^git
                cmd_clauses.append(f"status_detail LIKE '{_escape_sql_like(pattern[1:])}%' ESCAPE '\\'")
            else:
                # Contains (default)
                cmd_clauses.append(f"status_detail LIKE '%{_escape_sql_like(pattern)}%' ESCAPE '\\'")

        if len(cmd_clauses) == 1:
            clauses.append(cmd_clauses[0])
        else:
            clauses.append(f"({' OR '.join(cmd_clauses)})")

    # Message filters
    if "from" in filters:
        if len(filters["from"]) == 1:
            clauses.append(f"msg_from = '{_escape_sql(filters['from'][0])}'")
        else:
            froms = "', '".join(_escape_sql(x) for x in filters["from"])
            clauses.append(f"msg_from IN ('{froms}')")

    if "mention" in filters:
        mention_clauses = []
        for name in filters["mention"]:
            mention_clauses.append(f"msg_mentions LIKE '%{_escape_sql_like(name)}%' ESCAPE '\\'")

        if len(mention_clauses) == 1:
            clauses.append(mention_clauses[0])
        else:
            clauses.append(f"({' OR '.join(mention_clauses)})")

    if "intent" in filters:
        if len(filters["intent"]) == 1:
            clauses.append(f"msg_intent = '{_escape_sql(filters['intent'][0])}'")
        else:
            intents = "', '".join(_escape_sql(x) for x in filters["intent"])
            clauses.append(f"msg_intent IN ('{intents}')")

    if "thread" in filters:
        if len(filters["thread"]) == 1:
            clauses.append(f"msg_thread = '{_escape_sql(filters['thread'][0])}'")
        else:
            threads = "', '".join(_escape_sql(x) for x in filters["thread"])
            clauses.append(f"msg_thread IN ('{threads}')")

    if "reply_to" in filters:
        if len(filters["reply_to"]) == 1:
            clauses.append(f"msg_reply_to = '{_escape_sql(filters['reply_to'][0])}'")
        else:
            reply_tos = "', '".join(_escape_sql(x) for x in filters["reply_to"])
            clauses.append(f"msg_reply_to IN ('{reply_tos}')")

    # Life filters
    if "action" in filters:
        if len(filters["action"]) == 1:
            clauses.append(f"life_action = '{_escape_sql(filters['action'][0])}'")
        else:
            actions = "', '".join(_escape_sql(x) for x in filters["action"])
            clauses.append(f"life_action IN ('{actions}')")

    # Time range filters
    if "after" in filters:
        for ts in filters["after"]:
            clauses.append(f"timestamp >= '{_escape_sql(ts)}'")

    if "before" in filters:
        for ts in filters["before"]:
            clauses.append(f"timestamp < '{_escape_sql(ts)}'")

    # Collision filter (special case - complex EXISTS query)
    if "collision" in filters:
        collision_sql = f"""type = 'status' AND status_context IN {FILE_WRITE_CONTEXTS}
AND EXISTS (
    SELECT 1 FROM events_v e
    WHERE e.type = 'status' AND e.status_context IN {FILE_WRITE_CONTEXTS}
    AND e.status_detail = events_v.status_detail
    AND e.instance != events_v.instance
    AND ABS(strftime('%s', events_v.timestamp) - strftime('%s', e.timestamp)) < 20
)"""
        clauses.append(f"({collision_sql})")

    return " AND ".join(clauses)


def expand_shortcuts(argv: list[str]) -> list[str]:
    """Expand shortcut flags to full flags.

    Shortcuts provide convenient aliases for common filter combinations:
    - --idle NAME → --agent NAME --status listening
    - --blocked NAME → --agent NAME --status blocked
    - --collision → (passed through, handled in SQL generation)

    Args:
        argv: Command line arguments list

    Returns:
        Expanded argv list with shortcuts replaced

    Example:
        >>> expand_shortcuts(['--idle', 'peso'])
        ['--agent', 'peso', '--status', 'listening']
    """
    expanded = []
    i = 0

    while i < len(argv):
        if argv[i] == "--idle" and i + 1 < len(argv):
            expanded.extend(["--agent", argv[i + 1], "--status", ST_LISTENING])
            i += 2
        elif argv[i] == "--blocked" and i + 1 < len(argv):
            expanded.extend(["--agent", argv[i + 1], "--status", ST_BLOCKED])
            i += 2
        else:
            # Pass through (including --collision)
            expanded.append(argv[i])
            i += 1

    return expanded


# parse_timestamp removed - was unused and intended for Phase 6 (relative timestamps)
# If needed in future, add back with actual usage
