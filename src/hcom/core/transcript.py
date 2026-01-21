"""Thread - conversation context extraction and sharing."""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any, Callable

# Error detection patterns
ERROR_PATTERNS = re.compile(r"rejected|interrupted|error:|traceback|failed|exception", re.I)


# =============================================================================
# Content Extraction Helpers
# =============================================================================


def extract_text_content(content: str | list) -> str:
    """Extract text content from message content field."""
    if isinstance(content, str):
        return content.strip()
    if isinstance(content, list):
        parts = [
            block.get("text", "").strip()
            for block in content
            if block.get("type") == "text" and block.get("text", "").strip()
        ]
        return "\n".join(parts)
    return ""


def has_user_text(content: str | list) -> bool:
    """Check if content has actual user text (not just tool_result blocks)."""
    if isinstance(content, str):
        return bool(content.strip())
    if isinstance(content, list):
        return any(block.get("type") == "text" and block.get("text", "").strip() for block in content)
    return False


def extract_files_from_content(content: list | str) -> list[str]:
    """Extract file paths from assistant message content (tool_use blocks)."""
    if not isinstance(content, list):
        return []

    files = set()
    for block in content:
        if block.get("type") != "tool_use":
            continue

        tool_input = block.get("input", {})
        if not isinstance(tool_input, dict):
            continue

        # Common file path fields across tools
        for field in ("file_path", "path", "filePath", "notebook_path"):
            if field in tool_input:
                path = tool_input[field]
                if isinstance(path, str) and path:
                    files.add(Path(path).name)

        # Glob/Grep patterns - extract base path
        if "pattern" in tool_input and "path" not in tool_input:
            pattern = tool_input.get("pattern", "")
            if "/" in pattern:
                base = pattern.split("*")[0].rstrip("/")
                if base:
                    files.add(base + "/")

    return sorted(files)[:10]


def extract_tool_uses(content: list | str) -> list[dict]:
    """Extract tool_use blocks from assistant message content."""
    if not isinstance(content, list):
        return []
    return [
        {"id": b.get("id", ""), "name": b.get("name", ""), "input": b.get("input", {})}
        for b in content
        if b.get("type") == "tool_use"
    ]


def extract_tool_results(content: list | str) -> list[dict]:
    """Extract tool_result blocks from user message content."""
    if not isinstance(content, list):
        return []
    return [
        {
            "tool_use_id": b.get("tool_use_id", ""),
            "content": b.get("content", ""),
            "is_error": b.get("is_error", False),
        }
        for b in content
        if b.get("type") == "tool_result"
    ]


def is_error_result(result: dict) -> bool:
    """Check if a tool result indicates an error."""
    if result.get("is_error"):
        return True
    content = result.get("content", "")
    return isinstance(content, str) and bool(ERROR_PATTERNS.search(content))


# =============================================================================
# Edit/Bash Info Extraction
# =============================================================================


def format_structured_patch(patch: list) -> str:
    """Format structuredPatch into readable diff."""
    if not patch or not isinstance(patch, list):
        return ""

    lines = []
    for hunk in patch:
        if not isinstance(hunk, dict):
            continue
        old_start = hunk.get("oldStart", 0)
        new_start = hunk.get("newStart", 0)
        hunk_lines = hunk.get("lines", [])

        lines.append(f"@@ -{old_start} +{new_start} @@")
        lines.extend(hunk_lines[:20])
        if len(hunk_lines) > 20:
            lines.append(f"  ... +{len(hunk_lines) - 20} more lines")

    return "\n".join(lines)


def extract_edit_info(tool_use_result: dict | None) -> dict | None:
    """Extract edit information from toolUseResult."""
    if not tool_use_result or not isinstance(tool_use_result, dict):
        return None
    if "structuredPatch" not in tool_use_result and "oldString" not in tool_use_result:
        return None

    result = {"file": tool_use_result.get("filePath", "")}

    if "structuredPatch" in tool_use_result:
        result["diff"] = format_structured_patch(tool_use_result["structuredPatch"])
    elif "oldString" in tool_use_result and "newString" in tool_use_result:
        old = tool_use_result["oldString"]
        new = tool_use_result["newString"]
        old_preview = old[:100] + "..." if len(old) > 100 else old
        new_preview = new[:100] + "..." if len(new) > 100 else new
        result["diff"] = f"-{old_preview}\n+{new_preview}"

    return result


def extract_bash_info(tool_input: dict, tool_result_content: str) -> dict:
    """Extract bash command execution info."""
    output = tool_result_content
    if len(output) > 500:
        output = output[:500] + f"... (+{len(tool_result_content) - 500} chars)"
    return {
        "command": tool_input.get("command", ""),
        "description": tool_input.get("description", ""),
        "output": output,
    }


# =============================================================================
# Text Summarization
# =============================================================================


def summarize_action(text: str, max_len: int = 200) -> str:
    """Summarize assistant action from text content."""
    if not text:
        return "(no response)"

    total_len = len(text)
    lines = [line.strip() for line in text.split("\n") if line.strip()]
    if not lines:
        return "(no response)"

    # Strip common prefixes
    first = lines[0]
    for prefix in ("I'll ", "I will ", "Let me ", "Sure, ", "Okay, ", "OK, "):
        if first.startswith(prefix):
            first = first[len(prefix) :]
            break
    lines[0] = first

    summary = " ".join(lines[:3])
    if len(summary) > max_len:
        summary = summary[: max_len - 3] + "..."

    if total_len > len(summary) + 50:
        summary += f" (+{total_len - len(summary)} chars)"

    return summary


# =============================================================================
# Common Message Parsing
# =============================================================================


def _is_subagent_transcript(path: Path) -> bool:
    """Check if transcript is from a subagent (agent-*.jsonl)."""
    return path.name.startswith("agent-")


def _should_skip_entry(entry: dict, is_subagent: bool) -> bool:
    """Check if entry should be skipped based on meta flags."""
    if entry.get("isMeta"):
        return True
    if entry.get("isSidechain") and not is_subagent:
        return True
    if entry.get("isCompactSummary"):
        return True
    return False


def _parse_messages(path: Path, include_tool_use_result: bool = False) -> tuple[list[dict], str | None]:
    """Parse transcript JSONL into message list.

    Returns:
        (messages, error) - messages list and optional error string
    """
    if not path.exists():
        return [], f"Transcript not found: {path}"

    is_subagent = _is_subagent_transcript(path)
    messages = []

    try:
        with open(path, "r") as f:
            for line in f:
                if not line.strip():
                    continue
                try:
                    entry = json.loads(line)
                    msg_type = entry.get("type")
                    if msg_type not in ("user", "assistant"):
                        continue
                    if _should_skip_entry(entry, is_subagent):
                        continue

                    msg = {
                        "type": msg_type,
                        "content": entry.get("message", {}).get("content", ""),
                        "timestamp": entry.get("timestamp", ""),
                        "uuid": entry.get("uuid", ""),
                        "session_id": entry.get("sessionId", ""),
                    }
                    if include_tool_use_result:
                        msg["tool_use_result"] = entry.get("toolUseResult")

                    messages.append(msg)
                except json.JSONDecodeError:
                    continue
    except Exception as e:
        return [], f"Error reading transcript: {e}"

    return messages, None


def _find_user_messages(messages: list[dict]) -> list[dict]:
    """Filter messages to those with actual user text."""
    return [m for m in messages if m["type"] == "user" and has_user_text(m["content"])]


def _find_msg_index(messages: list[dict], uuid: str) -> int:
    """Find index of message by uuid."""
    return next((i for i, m in enumerate(messages) if m["uuid"] == uuid), -1)


def _collect_responses_until_next_prompt(messages: list[dict], start_idx: int) -> tuple[list[str], list[str]]:
    """Collect assistant text and files from messages after start_idx until next user prompt.

    Returns:
        (action_parts, files)
    """
    action_parts = []
    files = []

    for m in messages[start_idx + 1 :]:
        if m["type"] == "user" and has_user_text(m["content"]):
            break
        if m["type"] == "assistant":
            text = extract_text_content(m["content"])
            if text:
                action_parts.append(text)
            files.extend(extract_files_from_content(m["content"]))

    return action_parts, files


# =============================================================================
# Main Parsers
# =============================================================================


def parse_claude_thread(
    transcript_path: str | Path,
    last: int = 10,
    range_tuple: tuple[int, int] | None = None,
) -> dict:
    """Parse Claude Code transcript into structured exchanges.

    Args:
        transcript_path: Path to transcript JSONL
        last: Number of recent exchanges (ignored if range_tuple provided)
        range_tuple: (start, end) absolute positions, 1-indexed inclusive

    Returns:
        {"exchanges": [...], "total": int, "error": str | None}
    """
    path = Path(transcript_path)
    messages, error = _parse_messages(path)
    if error:
        return {"exchanges": [], "total": 0, "error": error}

    user_messages = _find_user_messages(messages)
    total = len(user_messages)

    # Select which messages to process
    if range_tuple:
        start, end = range_tuple
        selected = user_messages[start - 1 : end]  # 1-indexed to 0-indexed
        base_pos = start
    else:
        selected = user_messages[-last:]
        base_pos = max(1, total - last + 1)

    exchanges = []

    for idx, user_msg in enumerate(selected):
        user_text = extract_text_content(user_msg["content"])
        if not user_text:
            continue

        user_idx = _find_msg_index(messages, user_msg["uuid"])
        action = "(no response)"
        files: list[str] = []

        if user_idx >= 0:
            action_parts, files = _collect_responses_until_next_prompt(messages, user_idx)
            if action_parts:
                action = "\n".join(action_parts)
            files = sorted(set(files))[:5]

        exchanges.append(
            {
                "position": base_pos + idx,
                "user": user_text[:300],
                "action": action,
                "files": files,
                "timestamp": user_msg["timestamp"],
            }
        )

    return {"exchanges": exchanges, "total": total, "error": None}


def parse_claude_thread_detailed(
    transcript_path: str | Path,
    last: int = 10,
    range_tuple: tuple[int, int] | None = None,
) -> dict:
    """Parse Claude Code transcript with full tool I/O.

    Args:
        transcript_path: Path to transcript JSONL
        last: Number of recent exchanges (ignored if range_tuple provided)
        range_tuple: (start, end) absolute positions, 1-indexed inclusive

    Returns:
        {"exchanges": [...], "total": int, "error": str | None, "ended_on_error": bool}
    """
    path = Path(transcript_path)
    messages, error = _parse_messages(path, include_tool_use_result=True)
    if error:
        return {"exchanges": [], "total": 0, "error": error, "ended_on_error": False}

    # Build tool_use index: (session_id, tool_use_id) -> tool_use info
    tool_use_index: dict[tuple[str, str], dict] = {}
    for msg in messages:
        if msg["type"] == "assistant":
            session_id = msg["session_id"]
            for tool in extract_tool_uses(msg["content"]):
                tool_use_index[(session_id, tool["id"])] = tool

    user_messages = _find_user_messages(messages)
    total = len(user_messages)

    # Select which messages to process
    if range_tuple:
        start, end = range_tuple
        selected = user_messages[start - 1 : end]  # 1-indexed to 0-indexed
        base_pos = start
    else:
        selected = user_messages[-last:]
        base_pos = max(1, total - last + 1)

    exchanges = []

    for idx, user_msg in enumerate(selected):
        user_text = extract_text_content(user_msg["content"])
        if not user_text:
            continue

        user_idx = _find_msg_index(messages, user_msg["uuid"])
        exchange = _build_detailed_exchange(messages, user_idx, user_msg, user_text, tool_use_index)
        exchange["position"] = base_pos + idx
        exchanges.append(exchange)

    overall_ended_on_error = exchanges[-1]["ended_on_error"] if exchanges else False
    return {
        "exchanges": exchanges,
        "total": total,
        "error": None,
        "ended_on_error": overall_ended_on_error,
    }


def _build_detailed_exchange(
    messages: list[dict],
    user_idx: int,
    user_msg: dict,
    user_text: str,
    tool_use_index: dict[tuple[str, str], dict],
) -> dict:
    """Build a detailed exchange record with tool I/O."""
    action = "(no response)"
    files: list[str] = []
    tools: list[dict] = []
    edits: list[dict] = []
    errors: list[dict] = []
    last_was_error = False

    if user_idx >= 0:
        action_parts = []
        session_id = user_msg["session_id"]

        for m in messages[user_idx + 1 :]:
            if m["type"] == "user" and has_user_text(m["content"]):
                break

            if m["type"] == "assistant":
                text = extract_text_content(m["content"])
                if text:
                    action_parts.append(text)
                files.extend(extract_files_from_content(m["content"]))

            elif m["type"] == "user":
                # Tool result message
                tool_use_result = m.get("tool_use_result")
                for tr in extract_tool_results(m["content"]):
                    tool_record, edit_info, is_err = _process_tool_result(
                        tr, session_id, tool_use_index, tool_use_result
                    )
                    tools.append(tool_record)
                    if edit_info:
                        edits.append(edit_info)
                    if is_err:
                        errors.append(
                            {
                                "tool": tool_record["name"],
                                "content": tr.get("content", "")[:300],
                            }
                        )
                        last_was_error = True
                    else:
                        last_was_error = False

        if action_parts:
            action = "\n".join(action_parts)
        files = sorted(set(files))[:5]

    return {
        "user": user_text[:500],
        "action": action,
        "files": files,
        "timestamp": user_msg["timestamp"],
        "tools": tools,
        "edits": edits,
        "errors": errors,
        "ended_on_error": last_was_error,
    }


def _process_tool_result(
    tr: dict,
    session_id: str,
    tool_use_index: dict[tuple[str, str], dict],
    tool_use_result: Any,
) -> tuple[dict, dict | None, bool]:
    """Process a single tool result.

    Returns:
        (tool_record, edit_info or None, is_error)
    """
    tool_use_id = tr["tool_use_id"]
    tool_use = tool_use_index.get((session_id, tool_use_id), {})
    tool_name = tool_use.get("name", "unknown")
    tool_input = tool_use.get("input", {})
    is_err = is_error_result(tr)

    tool_record: dict[str, Any] = {"name": tool_name, "is_error": is_err}
    edit_info = None

    if tool_name == "Bash":
        bash_info = extract_bash_info(tool_input, tr.get("content", ""))
        tool_record["command"] = bash_info["command"]
        tool_record["output"] = bash_info["output"]
    elif tool_name == "Edit":
        edit_info = extract_edit_info(tool_use_result)
        if edit_info:
            tool_record["file"] = edit_info.get("file", "")
    elif tool_name in ("Read", "Glob", "Grep"):
        tool_record["target"] = tool_input.get("file_path") or tool_input.get("path") or tool_input.get("pattern", "")

    return tool_record, edit_info, is_err


# =============================================================================
# Formatters
# =============================================================================


def format_thread(thread_data: dict, instance: str = "", full: bool = False) -> str:
    """Format thread data for human-readable output."""
    exchanges = thread_data.get("exchanges", [])
    total = thread_data.get("total", len(exchanges))
    error = thread_data.get("error")

    if error:
        return f"Error: {error}"
    if not exchanges:
        return "No conversation exchanges found."

    # Build header with position info
    lines = []
    first_pos = exchanges[0].get("position", 1)
    last_pos = exchanges[-1].get("position", len(exchanges))
    header = f"Recent conversation ({len(exchanges)} exchanges, {first_pos}-{last_pos} of {total})"
    if instance:
        header += f" - @{instance}"
    lines.append(header + ":")
    lines.append("")

    for ex in exchanges:
        pos = ex.get("position", "?")
        user = ex["user"]
        if len(user) > 300:
            user = user[:297] + "..."
        lines.append(f"[{pos}] USER: {user}")

        action = ex["action"]
        if full:
            lines.append(f"ASSISTANT: {action}")
        else:
            lines.append(f"ASSISTANT: {summarize_action(action)}")

        if ex["files"]:
            lines.append(f"FILES: {', '.join(ex['files'])}")
        lines.append("")

    return "\n".join(lines).rstrip()


def format_thread_detailed(thread_data: dict, instance: str = "") -> str:
    """Format detailed thread data for watcher-style review."""
    exchanges = thread_data.get("exchanges", [])
    total = thread_data.get("total", len(exchanges))
    error = thread_data.get("error")
    ended_on_error = thread_data.get("ended_on_error", False)

    if error:
        return f"Error: {error}"
    if not exchanges:
        return "No conversation exchanges found."

    # Build header with position info
    lines = []
    first_pos = exchanges[0].get("position", 1)
    last_pos = exchanges[-1].get("position", len(exchanges))
    header = f"Detailed review ({len(exchanges)} exchanges, {first_pos}-{last_pos} of {total})"
    if instance:
        header += f" - @{instance}"
    if ended_on_error:
        header += " [ENDED ON ERROR]"
    lines.append(header)
    lines.append("=" * len(header))
    lines.append("")

    for ex in exchanges:
        pos = ex.get("position", "?")
        user = ex["user"]
        if len(user) > 100:
            user = user[:97] + "..."
        lines.append(f'[{pos}] "{user}"')

        # Tools executed
        for tool in ex.get("tools", []):
            _format_tool_line(lines, tool)

        # Edits with diffs
        for edit in ex.get("edits", []):
            _format_edit_lines(lines, edit)

        # Errors
        for err in ex.get("errors", []):
            _format_error_lines(lines, err)

        if ex.get("ended_on_error"):
            lines.append("  └─ [ENDED ON ERROR]")
        lines.append("")

    return "\n".join(lines).rstrip()


def _format_tool_line(lines: list[str], tool: dict) -> None:
    """Format a single tool execution line."""
    prefix = "  ✗" if tool.get("is_error") else "  ├─"
    name = tool.get("name", "unknown")

    if name == "Bash":
        cmd = tool.get("command", "")[:60]
        suffix = " → ERROR" if tool.get("is_error") else ""
        lines.append(f"{prefix} Bash: {cmd}{suffix}")
    elif name == "Edit":
        lines.append(f"{prefix} Edit: {tool.get('file', '')}")
    elif name in ("Read", "Glob", "Grep"):
        target = tool.get("target", "")
        if len(target) > 50:
            target = "..." + target[-47:]
        lines.append(f"{prefix} {name}: {target}")
    else:
        lines.append(f"{prefix} {name}")


def _format_edit_lines(lines: list[str], edit: dict) -> None:
    """Format edit diff lines."""
    lines.append(f"  │ Edit {edit.get('file', '')}:")
    diff = edit.get("diff", "")
    diff_split = diff.split("\n")
    for diff_line in diff_split[:10]:
        lines.append(f"  │   {diff_line}")
    if len(diff_split) > 10:
        lines.append(f"  │   ... +{len(diff_split) - 10} more lines")


def _format_error_lines(lines: list[str], err: dict) -> None:
    """Format error lines."""
    lines.append(f"  ✗ ERROR ({err.get('tool', 'unknown')}):")
    content = err.get("content", "")[:200]
    for err_line in content.split("\n")[:3]:
        lines.append(f"  ✗   {err_line}")


# =============================================================================
# Gemini/Codex Parsers
# =============================================================================

# Tool name normalization for cross-tool consistency
TOOL_ALIASES = {
    # Gemini tool names
    "run_shell_command": "Bash",
    "read_file": "Read",
    "write_file": "Write",
    "edit_file": "Edit",
    "search_files": "Grep",
    "list_files": "Glob",
    "list_directory": "Glob",
    # Codex tool names
    "shell": "Bash",
    "apply_patch": "Edit",
}


def _normalize_tool_name(name: str) -> str:
    """Normalize tool name to Claude-style for consistency."""
    # Strip prefixes like "default_api:" or "functions."
    if ":" in name:
        name = name.split(":")[-1]
    if "." in name:
        name = name.split(".")[-1]
    return TOOL_ALIASES.get(name, name)


def parse_gemini_thread(
    transcript_path: str | Path,
    last: int = 10,
    range_tuple: tuple[int, int] | None = None,
    detailed: bool = False,
) -> dict:
    """Parse Gemini CLI session JSON into structured exchanges.

    Gemini stores sessions as single JSON files with a messages array.
    Format: ~/.gemini/tmp/<hash>/chats/session-*.json

    Args:
        transcript_path: Path to session JSON file
        last: Number of recent exchanges (ignored if range_tuple provided)
        range_tuple: (start, end) absolute positions, 1-indexed inclusive
        detailed: If True, include tool usage details

    Returns:
        {"exchanges": [...], "total": int, "error": str | None}
    """
    path = Path(transcript_path)
    if not path.exists():
        return {"exchanges": [], "total": 0, "error": f"Transcript not found: {path}"}

    try:
        with open(path, "r") as f:
            data = json.load(f)
    except json.JSONDecodeError as e:
        return {"exchanges": [], "total": 0, "error": f"Invalid JSON: {e}"}
    except Exception as e:
        return {"exchanges": [], "total": 0, "error": f"Error reading file: {e}"}

    messages = data.get("messages", [])
    if not messages:
        return {"exchanges": [], "total": 0, "error": None}

    # Find user messages (prompts)
    user_messages = [m for m in messages if m.get("type") == "user"]
    total = len(user_messages)

    # Select which messages to process
    if range_tuple:
        start, end = range_tuple
        selected = user_messages[start - 1 : end]
        base_pos = start
    else:
        selected = user_messages[-last:]
        base_pos = max(1, total - last + 1)

    # Build message index for quick lookup
    msg_index = {m.get("id"): i for i, m in enumerate(messages)}

    exchanges = []
    for idx, user_msg in enumerate(selected):
        user_text = user_msg.get("content", "")
        if not user_text:
            continue

        # Find responses after this user message
        user_idx = msg_index.get(user_msg.get("id"), -1)
        action_parts = []
        files = []
        tools = []

        if user_idx >= 0:
            for m in messages[user_idx + 1 :]:
                if m.get("type") == "user":
                    break  # Stop at next user message
                if m.get("type") == "gemini":
                    # Extract text content
                    content = m.get("content", "")
                    if content:
                        action_parts.append(content)

                    # Extract tool calls
                    for tc in m.get("toolCalls", []):
                        raw_name = tc.get("name", "")
                        tool_name = _normalize_tool_name(raw_name)
                        args = tc.get("args", {})

                        # Extract file paths from common fields for summary
                        for field in ("file", "path", "file_path", "directory"):
                            if field in args:
                                files.append(Path(args[field]).name)

                        if detailed:
                            tool_record = {
                                "name": tool_name,
                                "is_error": False,
                            }  # Gemini JSON doesn't capture tool errors reliably yet

                            if tool_name == "Bash":
                                tool_record["command"] = args.get("command", "")
                                # Gemini transcript might store output in a separate toolResult message or field
                                # But for now we just capture the call args
                            elif tool_name == "Read":
                                tool_record["target"] = args.get("file_path", "")
                            elif tool_name == "Write":
                                tool_record["target"] = args.get("file_path", "")
                            elif tool_name == "Edit":
                                tool_record["file"] = args.get("file_path", "")
                            elif tool_name in ("Glob", "Grep"):
                                tool_record["target"] = args.get("dir_path") or args.get("pattern", "")

                            tools.append(tool_record)

        action = "\n".join(action_parts) if action_parts else "(no response)"
        files = sorted(set(files))[:5]

        exchange = {
            "position": base_pos + idx,
            "user": user_text[:300],
            "action": action,
            "files": files,
            "timestamp": user_msg.get("timestamp", ""),
        }

        if detailed:
            exchange["tools"] = tools
            # Edits/Errors not easily available in current Gemini JSON format
            exchange["edits"] = []
            exchange["errors"] = []

        exchanges.append(exchange)

    return {"exchanges": exchanges, "total": total, "error": None}


def parse_codex_thread(
    transcript_path: str | Path,
    last: int = 10,
    range_tuple: tuple[int, int] | None = None,
) -> dict:
    """Parse Codex CLI rollout JSONL into structured exchanges.

    Codex stores sessions as JSONL with different entry types.
    Format: ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl

    Args:
        transcript_path: Path to rollout JSONL file
        last: Number of recent exchanges (ignored if range_tuple provided)
        range_tuple: (start, end) absolute positions, 1-indexed inclusive

    Returns:
        {"exchanges": [...], "total": int, "error": str | None}
    """
    path = Path(transcript_path)
    if not path.exists():
        return {"exchanges": [], "total": 0, "error": f"Transcript not found: {path}"}

    # Parse all response_item entries
    messages = []
    try:
        with open(path, "r") as f:
            for line in f:
                if not line.strip():
                    continue
                try:
                    entry = json.loads(line)
                    if entry.get("type") != "response_item":
                        continue
                    payload = entry.get("payload", {})
                    if payload.get("type") != "message":
                        continue

                    role = payload.get("role")
                    if role not in ("user", "assistant"):
                        continue

                    # Extract text content from content array
                    content_parts = payload.get("content", [])
                    text = ""
                    for part in content_parts:
                        if isinstance(part, dict):
                            # input_text for user, output_text for assistant
                            text += part.get("text", "")
                        elif isinstance(part, str):
                            text += part

                    messages.append(
                        {
                            "type": "user" if role == "user" else "assistant",
                            "content": text.strip(),
                            "timestamp": entry.get("timestamp", ""),
                        }
                    )
                except json.JSONDecodeError:
                    continue
    except Exception as e:
        return {"exchanges": [], "total": 0, "error": f"Error reading file: {e}"}

    if not messages:
        return {"exchanges": [], "total": 0, "error": None}

    # Find user messages (prompts)
    user_messages = [m for m in messages if m.get("type") == "user"]
    total = len(user_messages)

    # Select which messages to process
    if range_tuple:
        start, end = range_tuple
        selected_indices = list(range(start - 1, min(end, total)))
        base_pos = start
    else:
        selected_indices = list(range(max(0, total - last), total))
        base_pos = max(1, total - last + 1)

    # Build message lookup by index
    user_msg_indices = [i for i, m in enumerate(messages) if m.get("type") == "user"]

    exchanges = []
    for pos_offset, user_pos in enumerate(selected_indices):
        if user_pos >= len(user_msg_indices):
            continue
        msg_idx = user_msg_indices[user_pos]
        user_msg = messages[msg_idx]
        user_text = user_msg.get("content", "")
        if not user_text:
            continue

        # Find responses after this user message
        action_parts = []
        next_user_idx = user_msg_indices[user_pos + 1] if user_pos + 1 < len(user_msg_indices) else len(messages)

        for m in messages[msg_idx + 1 : next_user_idx]:
            if m.get("type") == "assistant":
                content = m.get("content", "")
                if content:
                    action_parts.append(content)

        action = "\n".join(action_parts) if action_parts else "(no response)"

        exchanges.append(
            {
                "position": base_pos + pos_offset,
                "user": user_text[:300],
                "action": action,
                "files": [],  # Codex doesn't easily expose file paths in this format
                "timestamp": user_msg.get("timestamp", ""),
            }
        )

    return {"exchanges": exchanges, "total": total, "error": None}


# =============================================================================
# Public API
# =============================================================================


PARSERS: dict[str, Callable[..., dict[str, Any]]] = {
    "claude": parse_claude_thread,
    "claude_detailed": parse_claude_thread_detailed,
    "gemini": parse_gemini_thread,
    "codex": parse_codex_thread,
}


def get_timeline(
    instances: list[dict],
    last: int = 10,
    detailed: bool = False,
) -> dict:
    """Get unified timeline of exchanges across multiple transcripts.

    Uses mtime-based sorting to efficiently find most recent exchanges
    without parsing all transcripts.

    Args:
        instances: List of instance dicts with 'name', 'transcript_path', 'tool'
        last: Number of recent exchanges to return
        detailed: If True, include tool I/O details

    Returns:
        {"entries": [...], "error": str | None}
        Each entry has: instance, position, user, action, timestamp, command, files
    """
    import os

    # Filter to instances with transcript paths and get mtimes
    transcript_info = []
    for inst in instances:
        path = inst.get("transcript_path", "")
        if not path:
            continue
        try:
            mtime = os.path.getmtime(path)
            transcript_info.append(
                {
                    "name": inst.get("name", ""),
                    "path": path,
                    "tool": inst.get("tool", "claude"),
                    "mtime": mtime,
                }
            )
        except OSError:
            continue  # File doesn't exist or inaccessible

    if not transcript_info:
        return {"entries": [], "error": "No transcripts found"}

    # Sort by mtime descending (newest first)
    transcript_info.sort(key=lambda x: x["mtime"], reverse=True)

    # Collect exchanges from transcripts, newest files first
    all_entries = []
    for info in transcript_info:
        # Parse this transcript
        thread_data = get_thread(
            info["path"],
            last=last,  # Get up to 'last' from each initially
            tool=info["tool"],
            detailed=detailed,
        )

        if thread_data.get("error"):
            continue

        for ex in thread_data.get("exchanges", []):
            all_entries.append(
                {
                    "instance": info["name"],
                    "position": ex.get("position", 0),
                    "user": ex.get("user", ""),
                    "action": ex.get("action", ""),
                    "timestamp": ex.get("timestamp", ""),
                    "files": ex.get("files", []),
                    "command": f"hcom transcript @{info['name']} {ex.get('position', '')}",
                    # Include detailed fields if present
                    "tools": ex.get("tools", []) if detailed else [],
                    "edits": ex.get("edits", []) if detailed else [],
                    "errors": ex.get("errors", []) if detailed else [],
                }
            )

    if not all_entries:
        return {"entries": [], "error": None}

    # Sort all entries by timestamp descending
    all_entries.sort(key=lambda x: x["timestamp"], reverse=True)

    # Take the last N
    entries = all_entries[:last]

    # Reverse to show oldest first (chronological order)
    entries.reverse()

    return {"entries": entries, "error": None}


def format_timeline(timeline_data: dict, full: bool = False) -> str:
    """Format timeline data for human-readable output."""
    entries = timeline_data.get("entries", [])
    error = timeline_data.get("error")

    if error:
        return f"Error: {error}"
    if not entries:
        return "No conversation exchanges found."

    lines = [f"Timeline ({len(entries)} exchanges):", ""]

    for entry in entries:
        # Parse timestamp for display
        ts = entry.get("timestamp", "")
        if ts:
            # Extract time portion (HH:MM) from ISO timestamp
            try:
                time_part = ts.split("T")[1][:5] if "T" in ts else ts[:5]
            except (IndexError, TypeError):
                time_part = "??:??"
        else:
            time_part = "??:??"

        user = entry.get("user", "")
        if len(user) > 80:
            user = user[:77] + "..."

        lines.append(f'[{time_part}] "{user}"')

        action = entry.get("action", "")
        if full:
            # Show full action
            for action_line in action.split("\n")[:10]:
                lines.append(f"  {action_line}")
            if action.count("\n") > 10:
                lines.append(f"  ... (+{action.count(chr(10)) - 10} lines)")
        else:
            # Summarized action
            lines.append(f"  → {summarize_action(action, max_len=100)}")

        if entry.get("files"):
            lines.append(f"  Files: {', '.join(entry['files'][:5])}")

        lines.append(f"  {entry.get('command', '')}")
        lines.append("")

    return "\n".join(lines).rstrip()


def format_timeline_detailed(timeline_data: dict) -> str:
    """Format timeline data with tool details."""
    entries = timeline_data.get("entries", [])
    error = timeline_data.get("error")

    if error:
        return f"Error: {error}"
    if not entries:
        return "No conversation exchanges found."

    lines = [f"Timeline ({len(entries)} exchanges) [detailed]", "=" * 40, ""]

    for entry in entries:
        # Parse timestamp
        ts = entry.get("timestamp", "")
        try:
            time_part = ts.split("T")[1][:5] if "T" in ts else ts[:5]
        except (IndexError, TypeError):
            time_part = "??:??"

        user = entry.get("user", "")
        if len(user) > 100:
            user = user[:97] + "..."

        lines.append(f'[{time_part}] "{user}"')

        # Tools executed
        for tool in entry.get("tools", []):
            _format_tool_line(lines, tool)

        # Edits
        for edit in entry.get("edits", []):
            _format_edit_lines(lines, edit)

        # Errors
        for err in entry.get("errors", []):
            _format_error_lines(lines, err)

        lines.append(f"  {entry.get('command', '')}")
        lines.append("")

    return "\n".join(lines).rstrip()


def get_thread(
    transcript_path: str | Path,
    last: int = 10,
    tool: str = "claude",
    detailed: bool = False,
    range_tuple: tuple[int, int] | None = None,
) -> dict:
    """Get structured thread from transcript.

    Args:
        transcript_path: Path to transcript file
        last: Number of recent exchanges (ignored if range_tuple provided)
        tool: AI tool type ('claude', 'gemini', etc.)
        detailed: If True, use detailed parser with tool I/O
        range_tuple: (start, end) absolute positions, 1-indexed inclusive

    Returns:
        Thread data dict with 'exchanges', 'total', and optional 'error'
    """
    if detailed and tool == "claude":
        return parse_claude_thread_detailed(transcript_path, last, range_tuple)

    if tool == "gemini":
        return parse_gemini_thread(transcript_path, last, range_tuple, detailed=detailed)

    parser = PARSERS.get(tool, parse_claude_thread)
    return parser(transcript_path, last, range_tuple)


# =============================================================================
# Search
# =============================================================================


def _get_live_transcript_paths(agent_filter: str | None = None) -> list[str]:
    """Get transcript paths for currently alive agents."""
    from ..core.db import get_db, init_db

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
    parts = p.parts
    for i, part in enumerate(parts):
        if part == ".claude" and i + 1 < len(parts) and parts[i + 1] == "projects":
            return "claude"
        if part == ".gemini":
            return "gemini"
        if part == ".codex" and i + 1 < len(parts) and parts[i + 1] == "sessions":
            return "codex"
    return "unknown"


def _get_hcom_tracked_paths(agent_filter: str | None = None) -> list[str]:
    """Get transcript paths for all hcom-tracked agents (alive + stopped + archived)."""
    import sqlite3
    from ..core.db import get_db, init_db, DB_FILE
    from ..core.paths import hcom_path, ARCHIVE_DIR

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
    import sqlite3
    from ..core.db import get_db, init_db, DB_FILE
    from ..core.paths import hcom_path, ARCHIVE_DIR

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
) -> dict[str, Any]:
    """Search transcripts using ripgrep or grep.

    Args:
        pattern: Search pattern (regex).
        limit: Max results to return.
        agent_filter: 'claude', 'gemini', 'codex', or None.
        scope: 'live' (alive only), 'hcom' (all tracked), or 'all' (all on disk).

    Returns:
        Dict with 'results' (list), 'count' (int), and 'scope' (str).
        Each result has: path, line, agent, text, hcom_name (opt), hcom_session (opt).
    """
    import shutil
    import subprocess

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
            p = home / ".claude" / "projects"
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
        if use_rg:
            cmd = ["rg", "--json", "-m", str(limit * 10), pattern]
            if not is_file_list:
                cmd.extend(["--glob", "*.jsonl", "--glob", "*.json"])
            cmd.extend(search_paths)
            proc = subprocess.run(cmd, capture_output=True, text=True, timeout=120)

            for line in proc.stdout.splitlines():
                if len(results) >= limit:
                    break
                try:
                    data = json.loads(line)
                    if data.get("type") == "match":
                        match_data = data["data"]
                        path = match_data["path"]["text"]
                        line_num = match_data["line_number"]
                        text = match_data["lines"]["text"].strip()
                        agent = _agent_from_path(path)
                        if agent_filter and agent != agent_filter:
                            continue
                        results.append(
                            {
                                "path": path,
                                "line": line_num,
                                "agent": agent,
                                "text": text,
                            }
                        )
                except (json.JSONDecodeError, KeyError):
                    continue
        else:
            if is_file_list:
                cmd = ["grep", "-Hn", pattern] + search_paths
            else:
                cmd = ["grep", "-rHn", "--include=*.jsonl", "--include=*.json", pattern] + search_paths
            proc = subprocess.run(cmd, capture_output=True, text=True, timeout=120)

            for line in proc.stdout.splitlines():
                if len(results) >= limit:
                    break
                last_colon = line.rfind(":")
                if last_colon == -1:
                    continue
                second_last_colon = line.rfind(":", 0, last_colon)
                if second_last_colon == -1:
                    continue
                path = line[:second_last_colon]
                line_num_str = line[second_last_colon + 1 : last_colon]
                text = line[last_colon + 1 :]
                if not line_num_str.isdigit():
                    continue
                agent = _agent_from_path(path)
                if agent_filter and agent != agent_filter:
                    continue
                results.append(
                    {
                        "path": path,
                        "line": int(line_num_str),
                        "agent": agent,
                        "text": text,
                    }
                )

    except (subprocess.TimeoutExpired, Exception):
        return {"results": [], "count": 0, "scope": scope, "error": "Search failed"}

    if results:
        unique_paths = set(r["path"] for r in results)
        path_to_hcom = _correlate_paths_to_hcom(unique_paths)
        for r in results:
            if r["path"] in path_to_hcom:
                info = path_to_hcom[r["path"]]
                r["hcom_name"] = info["name"]
                r["hcom_session"] = info["session"]

    return {"results": results, "count": len(results), "scope": scope}
