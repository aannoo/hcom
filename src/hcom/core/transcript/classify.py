"""Classify raw transcript entries by role.

Pure functions, no imports beyond stdlib. Each classifier takes a raw JSON dict
and returns one of: "user", "assistant", "tool_call", "tool_result", "system",
"thinking", "unknown".

OpenCode classifier uses simple role field (tool calls are parts within
assistant messages, not separate entries).
"""


def classify_claude(entry: dict) -> str:
    """Classify a Claude Code transcript entry."""
    if not entry:
        return "unknown"

    # System-level entries (check before type-based routing)
    if entry.get("isMeta") or entry.get("isCompactSummary"):
        return "system"

    entry_type = entry.get("type")

    # isSidechain entries are system-level
    if entry.get("isSidechain"):
        return "system"

    if entry_type in ("summary", "system", "result", "progress", "file-history-snapshot", "saved_hook_context"):
        return "system"

    if entry_type == "user":
        content = entry.get("message", {}).get("content", "")
        if isinstance(content, list):
            # If any block is tool_result, classify as tool_result
            if any(b.get("type") == "tool_result" for b in content):
                return "tool_result"
        return "user"

    if entry_type == "assistant":
        content = entry.get("message", {}).get("content", "")
        if isinstance(content, list):
            types = {b.get("type") for b in content}
            if "tool_use" in types:
                return "tool_call"
            if types and types <= {"thinking"}:
                return "thinking"
        return "assistant"

    return "unknown"


def classify_gemini(entry: dict) -> str:
    """Classify a Gemini CLI transcript entry."""
    if not entry:
        return "unknown"

    entry_type = entry.get("type")

    if entry_type == "user":
        return "user"

    if entry_type in ("info", "error"):
        return "system"

    if entry_type == "gemini":
        if entry.get("toolCalls"):
            return "tool_call"
        return "assistant"

    # Some Gemini transcripts use role-based format
    role = entry.get("role")
    if role == "user":
        return "user"
    if role in ("model", "gemini", "assistant"):
        if entry.get("toolCalls"):
            return "tool_call"
        return "assistant"

    # Tool result entries
    if entry_type in ("toolResult", "tool") or entry.get("toolResults"):
        return "tool_result"

    # Function response entries
    if entry_type == "functionResponse":
        return "tool_result"

    return "unknown"


def classify_codex(entry: dict) -> str:
    """Classify a Codex CLI transcript entry (full JSONL wrapper)."""
    if not entry:
        return "unknown"

    entry_type = entry.get("type")
    payload = entry.get("payload", {})
    payload_type = payload.get("type") if isinstance(payload, dict) else None

    if entry_type == "response_item":
        if payload_type == "message":
            role = payload.get("role")
            if role == "user":
                return "user"
            if role == "assistant":
                return "assistant"
            # system or other roles
            return "system"
        if payload_type == "function_call":
            return "tool_call"
        if payload_type == "function_call_output":
            return "tool_result"
        if payload_type == "custom_tool_call":
            return "tool_call"
        if payload_type == "reasoning":
            return "thinking"

    if entry_type == "event_msg":
        if payload_type == "user_message":
            return "user"
        if payload_type == "agent_message":
            return "assistant"
        if payload_type == "agent_reasoning":
            return "thinking"
        if payload_type == "token_count":
            return "system"

    if entry_type in ("session_meta", "turn_context", "session_start", "session_end"):
        return "system"

    # Catch-all for other event_msg subtypes as system
    if entry_type == "event_msg":
        return "system"

    return "unknown"


def classify_opencode(data: dict) -> str:
    """Classify an OpenCode message by its role field.

    OpenCode messages have a simple 'role' field in the JSON data column.
    Tool calls and results are parts within assistant messages, not separate entries.
    """
    if not data:
        return "unknown"
    role = data.get("role", "")
    if role == "user":
        return "user"
    if role == "assistant":
        return "assistant"
    return "unknown"


def detect_agent(path: str) -> str | None:
    """Detect agent type from transcript file path."""
    if ".claude" in path:
        return "claude"
    if ".gemini" in path:
        return "gemini"
    if ".codex" in path:
        return "codex"
    # OpenCode DB path contains "opencode" in the directory structure
    if "opencode" in path:
        return "opencode"
    return None
