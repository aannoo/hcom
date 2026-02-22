"""Helpers for extracting data from raw transcript entry dicts.

Refactored from _transcript_old.py with bug fixes:
- #4: is_error_result false positives fixed with word boundaries
- #5: extract_edit_info fallback to tool_use input
- #6: codex_is_error checks both exit code AND patterns
- #7: extract_files_from_content removes internal [:10] cap
"""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any

# Error detection patterns - fixed with word boundaries to avoid false positives
# like "error handling" or "no errors found"
ERROR_PATTERNS = re.compile(
    r"\b(rejected|interrupted|traceback|failed|exception)\b"
    r"|(?<!\w)error:"
    r"|command failed with exit code"
    r"|Traceback \(most recent call last\)",
    re.I,
)

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
    "shell_command": "Bash",
    "apply_patch": "Edit",
    # OpenCode tool names (lowercase)
    "bash": "Bash",
    "edit": "Edit",
    "read": "Read",
    "glob": "Glob",
    "grep": "Grep",
    "write": "Write",
    "skill": "Skill",
    "fetch": "WebFetch",
    "todoreplace": "Todo",
}


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
            if isinstance(block, dict) and block.get("type") == "text" and block.get("text", "").strip()
        ]
        return "\n".join(parts)
    return ""


def has_user_text(content: str | list) -> bool:
    """Check if content has actual user text (not just tool_result blocks)."""
    if isinstance(content, str):
        return bool(content.strip())
    if isinstance(content, list):
        return any(
            isinstance(block, dict) and block.get("type") == "text" and block.get("text", "").strip()
            for block in content
        )
    return False


def extract_files_from_content(content: list | str) -> list[str]:
    """Extract file paths from assistant message content (tool_use blocks).

    Bug fix #7: Removed internal [:10] cap. Callers should cap as needed.
    """
    if not isinstance(content, list):
        return []

    files = set()
    for block in content:
        if not isinstance(block, dict) or block.get("type") != "tool_use":
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

    return sorted(files)


def extract_tool_uses(content: list | str) -> list[dict]:
    """Extract tool_use blocks from assistant message content."""
    if not isinstance(content, list):
        return []
    return [
        {"id": b.get("id", ""), "name": b.get("name", ""), "input": b.get("input", {})}
        for b in content
        if isinstance(b, dict) and b.get("type") == "tool_use"
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
        if isinstance(b, dict) and b.get("type") == "tool_result"
    ]


def is_error_result(result: dict) -> bool:
    """Check if a tool result indicates an error.

    Bug fix #4: Uses word boundaries to avoid false positives like
    "error handling code" or "no errors found".
    """
    if result.get("is_error"):
        return True
    content = result.get("content", "")
    if not isinstance(content, str) or not content:
        return False
    # Only check first 500 chars to avoid scanning huge outputs
    return bool(ERROR_PATTERNS.search(content[:500]))


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


def extract_edit_info(
    tool_use_result: dict | None,
    tool_use_input: dict | None = None,
) -> dict | None:
    """Extract edit information from toolUseResult, with fallback to tool_use input.

    Bug fix #5: When tool_use_result is absent or lacks patch/old/new data,
    falls back to tool_use_input containing old_string/new_string/file_path.
    """
    if tool_use_result and isinstance(tool_use_result, dict):
        if "structuredPatch" in tool_use_result or "oldString" in tool_use_result:
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

    # Fallback: extract from tool_use input
    if tool_use_input and isinstance(tool_use_input, dict):
        if "old_string" in tool_use_input or "new_string" in tool_use_input:
            result = {"file": tool_use_input.get("file_path", "")}
            old = tool_use_input.get("old_string", "")
            new = tool_use_input.get("new_string", "")
            old_preview = old[:100] + "..." if len(old) > 100 else old
            new_preview = new[:100] + "..." if len(new) > 100 else new
            result["diff"] = f"-{old_preview}\n+{new_preview}"
            return result

    return None


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


def normalize_tool_name(name: str) -> str:
    """Normalize tool name by stripping namespace prefixes and applying aliases."""
    if ":" in name:
        name = name.split(":")[-1]
    if "." in name:
        name = name.split(".")[-1]
    return TOOL_ALIASES.get(name, name)


def codex_is_error(output: str) -> bool:
    """Check if Codex tool output indicates an error.

    Bug fix #6: Checks exit code AND patterns independently (not if/elif),
    so both conditions are evaluated.
    """
    if not output:
        return False
    is_err = False
    if output.startswith("Exit code:"):
        exit_line = output.split("\n")[0]
        if "Exit code: 0" not in exit_line:
            is_err = True
    # ALSO check patterns (not elif)
    if not is_err and ERROR_PATTERNS.search(output[:200]):
        is_err = True
    return is_err


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
            first = first[len(prefix):]
            break
    lines[0] = first

    summary = " ".join(lines[:3])
    if len(summary) > max_len:
        summary = summary[: max_len - 3] + "..."

    if total_len > len(summary) + 50:
        summary += f" (+{total_len - len(summary)} chars)"

    return summary


# =============================================================================
# Unified entry presentation
# =============================================================================


def present_entry(raw: dict, role: str, agent: str) -> dict:
    """Best-effort field extraction from raw transcript entry. Never raises.

    Returns a dict with all available fields. Missing fields are omitted.
    Always includes: role, timestamp, text, has_user_text.
    """
    result: dict[str, Any] = {"role": role, "timestamp": "", "text": "", "has_user_text": False, "files": []}

    try:
        if agent == "claude":
            result.update(_present_claude(raw, role))
        elif agent == "gemini":
            result.update(_present_gemini(raw, role))
        elif agent == "codex":
            result.update(_present_codex(raw, role))
        elif agent == "opencode":
            result.update(_present_opencode(raw, role))
    except Exception:
        pass  # partial result on failure

    return result


def _present_claude(raw: dict, role: str) -> dict:
    """Extract fields from a Claude transcript entry."""
    result: dict[str, Any] = {}
    result["timestamp"] = raw.get("timestamp", "")
    result["session_id"] = raw.get("sessionId", "")

    content = raw.get("message", {}).get("content", "")

    if role == "user":
        result["text"] = extract_text_content(content)
        result["has_user_text"] = has_user_text(content)
        result["tool_results"] = extract_tool_results(content)
        result["tool_use_result"] = raw.get("toolUseResult")
    elif role in ("assistant", "tool_call"):
        result["text"] = extract_text_content(content)
        result["has_user_text"] = False
        result["files"] = extract_files_from_content(content)
        result["tool_uses"] = extract_tool_uses(content)
    elif role == "tool_result":
        # Claude tool_result entries are type=user with tool_result content blocks
        result["text"] = extract_text_content(content)
        result["has_user_text"] = has_user_text(content)
        result["tool_results"] = extract_tool_results(content)
        result["tool_use_result"] = raw.get("toolUseResult")
    else:
        result["text"] = extract_text_content(content) if content else ""
        result["has_user_text"] = False

    return result


def _present_gemini(raw: dict, role: str) -> dict:
    """Extract fields from a Gemini transcript entry."""
    result: dict[str, Any] = {}
    result["timestamp"] = raw.get("timestamp", "")

    content = raw.get("content", "")
    if isinstance(content, str):
        result["text"] = content.strip()
    else:
        result["text"] = ""

    result["has_user_text"] = bool(result["text"]) if role == "user" else False

    if role in ("assistant", "tool_call"):
        tool_calls_raw = raw.get("toolCalls", [])
        if tool_calls_raw:
            tool_calls = []
            files = []
            for tc in tool_calls_raw:
                raw_name = tc.get("name", "")
                tool_name = normalize_tool_name(raw_name)
                args = tc.get("args", {})
                tool_calls.append({"name": tool_name, "args": args, "raw_name": raw_name})
                for field in ("file", "path", "file_path", "directory"):
                    if field in args:
                        val = args[field]
                        if isinstance(val, str) and val:
                            files.append(Path(val).name)
            result["tool_calls"] = tool_calls
            result["files"] = sorted(set(files))

    return result


def _present_codex(raw: dict, role: str) -> dict:
    """Extract fields from a Codex transcript entry."""
    result: dict[str, Any] = {}
    result["timestamp"] = raw.get("timestamp", "")

    payload = raw.get("payload", {})
    payload_type = payload.get("type", "")

    if payload_type == "message":
        content_parts = payload.get("content", [])
        text_parts = []
        for part in content_parts:
            if isinstance(part, dict):
                text_parts.append(part.get("text", ""))
            elif isinstance(part, str):
                text_parts.append(part)
        result["text"] = "".join(text_parts).strip()
        result["has_user_text"] = bool(result["text"]) if role == "user" else False

    elif payload_type == "function_call":
        result["text"] = ""
        result["has_user_text"] = False
        args_str = payload.get("arguments", "{}")
        try:
            args = json.loads(args_str) if isinstance(args_str, str) else args_str
        except (json.JSONDecodeError, ValueError):
            args = {}
        raw_name = payload.get("name", "unknown")
        result["fn_call"] = {
            "call_id": payload.get("call_id", ""),
            "name": raw_name,
            "tool_name": normalize_tool_name(raw_name),
            "arguments": args,
        }
        # Extract files from function call args
        files = []
        if isinstance(args, dict):
            for field in ("file_path", "path", "file"):
                if field in args:
                    val = args[field]
                    if isinstance(val, str) and val:
                        files.append(Path(val).name)
        result["files"] = files

    elif payload_type == "function_call_output":
        result["text"] = ""
        result["has_user_text"] = False
        result["fn_output"] = {
            "call_id": payload.get("call_id", ""),
            "output": payload.get("output", ""),
        }
    else:
        result["text"] = ""
        result["has_user_text"] = False

    return result


def _present_opencode(raw: dict, role: str) -> dict:
    """Extract fields from an OpenCode message dict with its parts.

    OpenCode raw dict has: {id, role, data, parts, time_created}.
    Parts are already parsed dicts with {type, text, tool, state, synthetic, ...}.
    """
    result: dict[str, Any] = {}
    result["timestamp"] = raw.get("timestamp", "")

    parts = raw.get("parts", [])

    if role == "user":
        # Extract text from non-synthetic TextParts
        text_parts = []
        for p in parts:
            if p.get("type") == "text" and not p.get("synthetic"):
                t = p.get("text", "")
                if t:
                    text_parts.append(t)
        result["text"] = "\n".join(text_parts)
        result["has_user_text"] = bool(text_parts)

    elif role == "assistant":
        # Extract text from TextParts
        text_parts = []
        files = []
        for p in parts:
            if p.get("type") == "text":
                t = p.get("text", "")
                if t:
                    text_parts.append(t)
            elif p.get("type") == "tool":
                state = p.get("state", {})
                tool_input = state.get("input", {})
                if isinstance(tool_input, dict):
                    for field in ("file_path", "filePath", "path", "pattern", "file"):
                        if field in tool_input:
                            val = tool_input[field]
                            if isinstance(val, str) and val:
                                files.append(Path(val).name)
        result["text"] = "\n".join(text_parts)
        result["has_user_text"] = False
        result["files"] = sorted(set(files))

    else:
        result["text"] = ""
        result["has_user_text"] = False

    return result
