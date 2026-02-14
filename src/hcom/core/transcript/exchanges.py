"""Exchange-based transcript parsing.

Groups transcript entries into "exchanges" (user prompt -> assistant responses)
using TranscriptIndex for efficient on-demand access. One unified builder
replaces the 3 separate agent-specific parsers.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any, Callable

from .entries import (
    codex_is_error,
    extract_bash_info,
    extract_edit_info,
    extract_files_from_content,
    extract_text_content,
    extract_tool_results,
    has_user_text,
    is_error_result,
    normalize_tool_name,
    present_entry,
)
from .index import TranscriptIndex


# =============================================================================
# Internal helpers
# =============================================================================


def _build_claude_tool_use_index(
    index: TranscriptIndex,
) -> dict[tuple[str, str], dict]:
    """Build (session_id, tool_use_id) -> tool_use dict for Claude detailed mode."""
    tool_use_idx: dict[tuple[str, str], dict] = {}
    for ie in index:
        if ie.role != "tool_call":
            continue
        raw = index.read_raw(ie)
        session_id = raw.get("sessionId", "")
        content = raw.get("message", {}).get("content", "")
        if not isinstance(content, list):
            continue
        for block in content:
            if isinstance(block, dict) and block.get("type") == "tool_use":
                tool_use_idx[(session_id, block.get("id", ""))] = {
                    "name": block.get("name", ""),
                    "input": block.get("input", {}),
                }
    return tool_use_idx


def _build_codex_call_outputs(index: TranscriptIndex) -> dict[str, str]:
    """Build call_id -> output dict for Codex."""
    outputs: dict[str, str] = {}
    for ie in index:
        if ie.role != "tool_result":
            continue
        raw = index.read_raw(ie)
        payload = raw.get("payload", {})
        if payload.get("type") == "function_call_output":
            outputs[payload.get("call_id", "")] = payload.get("output", "")
    return outputs


def _process_claude_tool_result(
    tr: dict,
    session_id: str,
    tool_use_index: dict[tuple[str, str], dict],
    tool_use_result: Any,
) -> tuple[dict, dict | None, bool]:
    """Process a single Claude tool_result block into a tool record."""
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
        edit_info = extract_edit_info(tool_use_result, tool_input)
        if edit_info:
            tool_record["file"] = edit_info.get("file", "")
    elif tool_name in ("Read", "Glob", "Grep"):
        tool_record["target"] = tool_input.get("file_path") or tool_input.get("path") or tool_input.get("pattern", "")

    return tool_record, edit_info, is_err


def _build_exchange_claude(
    index: TranscriptIndex,
    all_entries: list,
    user_pos: int,
    next_user_pos: int,
    position: int,
    detailed: bool,
    tool_use_index: dict[tuple[str, str], dict] | None,
) -> dict:
    """Build a single exchange dict for Claude."""
    user_raw = index.read_raw(all_entries[user_pos])
    user_content = user_raw.get("message", {}).get("content", "")
    user_text = extract_text_content(user_content)
    timestamp = user_raw.get("timestamp", "")

    action_parts: list[str] = []
    files: list[str] = []
    tools: list[dict] = []
    edits: list[dict] = []
    errors: list[dict] = []
    last_was_error = False

    session_id = user_raw.get("sessionId", "")

    for i in range(user_pos + 1, next_user_pos):
        ie = all_entries[i]
        if ie.role in ("assistant", "tool_call"):
            raw = index.read_raw(ie)
            content = raw.get("message", {}).get("content", "")
            text = extract_text_content(content)
            if text:
                action_parts.append(text)
            files.extend(extract_files_from_content(content))

        if detailed and ie.role == "tool_result" and tool_use_index is not None:
            raw = index.read_raw(ie)
            tool_use_result = raw.get("toolUseResult")
            content = raw.get("message", {}).get("content", "")
            for tr in extract_tool_results(content):
                tool_record, edit_info, is_err = _process_claude_tool_result(
                    tr, session_id, tool_use_index, tool_use_result
                )
                tools.append(tool_record)
                if edit_info:
                    edits.append(edit_info)
                if is_err:
                    raw_content = tr.get("content", "")
                    if not isinstance(raw_content, str):
                        raw_content = extract_text_content(raw_content) if isinstance(raw_content, list) else str(raw_content)
                    errors.append({
                        "tool": tool_record["name"],
                        "content": raw_content[:300],
                    })
                    last_was_error = True
                else:
                    last_was_error = False

    action = "\n".join(action_parts) if action_parts else "(no response)"
    files = sorted(set(files))[:5]

    exchange: dict = {
        "position": position,
        "user": user_text[:500 if detailed else 300],
        "action": action,
        "files": files,
        "timestamp": timestamp,
    }

    if detailed:
        exchange["tools"] = tools
        exchange["edits"] = edits
        exchange["errors"] = errors
        exchange["ended_on_error"] = last_was_error

    return exchange


def _build_exchange_gemini(
    index: TranscriptIndex,
    all_entries: list,
    user_pos: int,
    next_user_pos: int,
    position: int,
    detailed: bool,
) -> dict:
    """Build a single exchange dict for Gemini."""
    user_raw = index.read_raw(all_entries[user_pos])
    user_text = user_raw.get("content", "")
    if not isinstance(user_text, str):
        user_text = ""
    timestamp = user_raw.get("timestamp", "")

    action_parts: list[str] = []
    files: list[str] = []
    tools: list[dict] = []

    for i in range(user_pos + 1, next_user_pos):
        ie = all_entries[i]
        if ie.role not in ("assistant", "tool_call"):
            continue
        raw = index.read_raw(ie)
        content = raw.get("content", "")
        if content and isinstance(content, str):
            action_parts.append(content)

        for tc in raw.get("toolCalls", []):
            raw_name = tc.get("name", "")
            tool_name = normalize_tool_name(raw_name)
            args = tc.get("args", {})

            for field in ("file", "path", "file_path", "directory"):
                if field in args and isinstance(args[field], str) and args[field]:
                    files.append(Path(args[field]).name)

            if detailed:
                tool_record: dict = {"name": tool_name, "is_error": False}
                if tool_name == "Bash":
                    tool_record["command"] = args.get("command", "")
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

    exchange: dict = {
        "position": position,
        "user": user_text[:300],
        "action": action,
        "files": files,
        "timestamp": timestamp,
    }

    if detailed:
        exchange["tools"] = tools
        exchange["edits"] = []
        exchange["errors"] = []

    return exchange


def _build_exchange_codex(
    index: TranscriptIndex,
    all_entries: list,
    user_pos: int,
    next_user_pos: int,
    position: int,
    detailed: bool,
    call_outputs: dict[str, str],
) -> dict:
    """Build a single exchange dict for Codex."""
    user_p = present_entry(index.read_raw(all_entries[user_pos]), "user", "codex")
    user_text = user_p.get("text", "")
    timestamp = user_p.get("timestamp", "")

    action_parts: list[str] = []
    files: list[str] = []
    tools: list[dict] = []

    for i in range(user_pos + 1, next_user_pos):
        ie = all_entries[i]
        raw = index.read_raw(ie)
        p = present_entry(raw, ie.role, "codex")

        if ie.role == "assistant":
            text = p.get("text", "")
            if text:
                action_parts.append(text)

        elif ie.role == "tool_call":
            fn = p.get("fn_call")
            if fn:
                raw_name = fn["name"]
                tool_name = fn["tool_name"]
                args = fn.get("arguments", {})
                call_id = fn.get("call_id", "")

                # Extract files from function call args
                if isinstance(args, dict):
                    for field in ("file_path", "path", "file"):
                        if field in args:
                            val = args[field]
                            if isinstance(val, str) and val:
                                files.append(Path(val).name)

                if detailed:
                    output = call_outputs.get(call_id, "")
                    is_err = codex_is_error(output)
                    tool_record: dict = {"name": tool_name, "is_error": is_err}

                    if tool_name == "Bash" or raw_name in ("shell", "shell_command"):
                        tool_record["command"] = args.get("command", "")
                        if len(output) > 500:
                            output = output[:500] + f"... (+{len(output) - 500} chars)"
                        tool_record["output"] = output
                    elif tool_name == "Edit" or raw_name == "apply_patch":
                        tool_record["file"] = args.get("file_path") or args.get("path", "")
                    elif tool_name in ("Read", "Glob", "Grep"):
                        tool_record["target"] = args.get("file_path") or args.get("path") or args.get("pattern", "")

                    tools.append(tool_record)

    action = "\n".join(action_parts) if action_parts else "(no response)"
    files = sorted(set(files))[:5]

    exchange: dict = {
        "position": position,
        "user": user_text[:500 if detailed else 300],
        "action": action,
        "files": files,
        "timestamp": timestamp,
    }

    if detailed:
        exchange["tools"] = tools
        exchange["edits"] = []
        exchange["errors"] = [
            {"tool": t["name"], "content": t.get("output", "")[:300]} for t in tools if t.get("is_error")
        ]

    return exchange


# =============================================================================
# Public API
# =============================================================================


def get_exchanges(
    transcript_path: str | Path,
    agent: str = "claude",
    last: int = 10,
    range_tuple: tuple[int, int] | None = None,
    detailed: bool = False,
) -> dict:
    """Parse transcript into structured exchanges.

    Args:
        transcript_path: Path to transcript file
        agent: Agent type ('claude', 'gemini', 'codex')
        last: Number of recent exchanges (ignored if range_tuple provided)
        range_tuple: (start, end) absolute positions, 1-indexed inclusive
        detailed: If True, include tool usage details

    Returns:
        {"exchanges": [...], "total": int, "error": str | None}
        Detailed mode adds "ended_on_error": bool (Claude only at top level)
    """
    path = Path(transcript_path)
    if not path.exists():
        result: dict[str, Any] = {"exchanges": [], "total": 0, "error": f"Transcript not found: {path}"}
        if detailed and agent == "claude":
            result["ended_on_error"] = False
        return result

    try:
        index = TranscriptIndex.build(str(path), agent)
    except (json.JSONDecodeError, OSError, AttributeError, TypeError) as e:
        err_msg = f"Invalid JSON: {e}" if "json" in str(e).lower() else f"Error reading file: {e}"
        return {"exchanges": [], "total": 0, "error": err_msg}

    if not len(index):
        result = {"exchanges": [], "total": 0, "error": None}
        if detailed and agent == "claude":
            result["ended_on_error"] = False
        return result

    all_entries = list(index)

    # Find user entries (with actual text for Claude, content check for others)
    user_indices: list[int] = []
    for i, ie in enumerate(all_entries):
        if ie.role != "user":
            continue
        if agent == "claude":
            raw = index.read_raw(ie)
            content = raw.get("message", {}).get("content", "")
            if has_user_text(content):
                user_indices.append(i)
        elif agent == "gemini":
            raw = index.read_raw(ie)
            content = raw.get("content", "")
            if content and isinstance(content, str):
                user_indices.append(i)
        elif agent == "codex":
            raw = index.read_raw(ie)
            p = present_entry(raw, "user", "codex")
            if p.get("text", ""):
                user_indices.append(i)

    total = len(user_indices)

    if range_tuple:
        start, end = range_tuple
        selected_user_indices = user_indices[start - 1:end]
        base_pos = start
    else:
        selected_user_indices = user_indices[-last:]
        base_pos = max(1, total - last + 1)

    # Pre-build lookup tables for detailed mode
    tool_use_index = None
    call_outputs = None
    if detailed and agent == "claude":
        tool_use_index = _build_claude_tool_use_index(index)
    if agent == "codex":
        call_outputs = _build_codex_call_outputs(index)

    # Build lookup: for each user_indices position, what's the next user position?
    # user_indices is sorted (entries are in file order)
    num_all = len(all_entries)
    _next_user_pos: dict[int, int] = {}
    for k in range(len(user_indices)):
        _next_user_pos[user_indices[k]] = user_indices[k + 1] if k + 1 < len(user_indices) else num_all

    exchanges: list[dict] = []
    for idx, user_pos in enumerate(selected_user_indices):
        next_user_pos = _next_user_pos.get(user_pos, num_all)

        if agent == "claude":
            ex = _build_exchange_claude(
                index, all_entries, user_pos, next_user_pos,
                base_pos + idx, detailed, tool_use_index,
            )
        elif agent == "gemini":
            ex = _build_exchange_gemini(
                index, all_entries, user_pos, next_user_pos,
                base_pos + idx, detailed,
            )
        elif agent == "codex":
            ex = _build_exchange_codex(
                index, all_entries, user_pos, next_user_pos,
                base_pos + idx, detailed, call_outputs or {},
            )
        else:
            continue

        exchanges.append(ex)

    result = {"exchanges": exchanges, "total": total, "error": None}
    if detailed and agent == "claude":
        result["ended_on_error"] = exchanges[-1]["ended_on_error"] if exchanges else False
    return result


# =============================================================================
# Backward-compatible wrappers
# =============================================================================


def parse_claude_thread(transcript_path: str | Path, last: int = 10, range_tuple: tuple[int, int] | None = None) -> dict:
    return get_exchanges(transcript_path, "claude", last, range_tuple)


def parse_claude_thread_detailed(transcript_path: str | Path, last: int = 10, range_tuple: tuple[int, int] | None = None) -> dict:
    return get_exchanges(transcript_path, "claude", last, range_tuple, detailed=True)


def parse_gemini_thread(transcript_path: str | Path, last: int = 10, range_tuple: tuple[int, int] | None = None, detailed: bool = False) -> dict:
    return get_exchanges(transcript_path, "gemini", last, range_tuple, detailed)


def parse_codex_thread(transcript_path: str | Path, last: int = 10, range_tuple: tuple[int, int] | None = None, detailed: bool = False) -> dict:
    return get_exchanges(transcript_path, "codex", last, range_tuple, detailed)


def get_thread(
    transcript_path: str | Path,
    last: int = 10,
    tool: str = "claude",
    detailed: bool = False,
    range_tuple: tuple[int, int] | None = None,
) -> dict:
    return get_exchanges(transcript_path, tool, last, range_tuple, detailed)


def get_timeline(
    instances: list[dict],
    last: int = 10,
    detailed: bool = False,
) -> dict:
    """Get unified timeline of exchanges across multiple transcripts.

    Args:
        instances: List of instance dicts with 'name', 'transcript_path', 'tool'
        last: Number of recent exchanges to return
        detailed: If True, include tool I/O details

    Returns:
        {"entries": [...], "error": str | None}
    """
    transcript_info = []
    for inst in instances:
        path = inst.get("transcript_path", "")
        if not path:
            continue
        try:
            mtime = os.path.getmtime(path)
            transcript_info.append({
                "name": inst.get("name", ""),
                "path": path,
                "tool": inst.get("tool", "claude"),
                "mtime": mtime,
            })
        except OSError:
            continue

    if not transcript_info:
        return {"entries": [], "error": "No transcripts found"}

    transcript_info.sort(key=lambda x: x["mtime"], reverse=True)

    all_timeline_entries = []
    for info in transcript_info:
        thread_data = get_thread(
            info["path"],
            last=last,
            tool=info["tool"],
            detailed=detailed,
        )

        if thread_data.get("error"):
            continue

        for ex in thread_data.get("exchanges", []):
            all_timeline_entries.append({
                "instance": info["name"],
                "position": ex.get("position", 0),
                "user": ex.get("user", ""),
                "action": ex.get("action", ""),
                "timestamp": ex.get("timestamp", ""),
                "files": ex.get("files", []),
                "command": f"hcom transcript @{info['name']} {ex.get('position', '')}",
                "tools": ex.get("tools", []) if detailed else [],
                "edits": ex.get("edits", []) if detailed else [],
                "errors": ex.get("errors", []) if detailed else [],
            })

    if not all_timeline_entries:
        return {"entries": [], "error": None}

    all_timeline_entries.sort(key=lambda x: x["timestamp"], reverse=True)
    entries = all_timeline_entries[:last]
    entries.reverse()

    return {"entries": entries, "error": None}


# Backward-compat PARSERS dict
PARSERS: dict[str, Callable[..., dict[str, Any]]] = {
    "claude": parse_claude_thread,
    "claude_detailed": parse_claude_thread_detailed,
    "gemini": parse_gemini_thread,
    "codex": parse_codex_thread,
}
