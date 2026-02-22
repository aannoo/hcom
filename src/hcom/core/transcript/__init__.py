"""Transcript parsing package.

Modular transcript parsing for Claude, Gemini, and Codex transcripts.
"""

# Paths
from .paths import get_claude_config_dir, derive_gemini_transcript_path, derive_codex_transcript_path, get_opencode_db_path

# Content extraction helpers
from .entries import (
    extract_text_content,
    has_user_text,
    extract_files_from_content,
    extract_tool_uses,
    extract_tool_results,
    is_error_result,
    extract_edit_info,
    extract_bash_info,
    format_structured_patch,
    summarize_action,
    normalize_tool_name,
    present_entry,
    TOOL_ALIASES,
    ERROR_PATTERNS,
)

# Classifiers
from .classify import classify_claude, classify_gemini, classify_codex, classify_opencode, detect_agent

# Index
from .index import TranscriptIndex, IndexEntry

# Exchanges / Parsers
from .exchanges import (
    get_exchanges,
    parse_claude_thread,
    parse_claude_thread_detailed,
    parse_gemini_thread,
    parse_codex_thread,
    get_thread,
    get_timeline,
    PARSERS,
)

# Formatters
from .format import (
    format_thread,
    format_thread_detailed,
    format_timeline,
    format_timeline_detailed,
)

# Search
from .search import (
    search_transcripts,
    _agent_from_path,
    _get_live_transcript_paths,
    _get_hcom_tracked_paths,
    _correlate_paths_to_hcom,
)

__all__ = [
    # Paths
    "get_claude_config_dir",
    "derive_gemini_transcript_path",
    "derive_codex_transcript_path",
    "get_opencode_db_path",
    # Content extraction
    "extract_text_content",
    "has_user_text",
    "extract_files_from_content",
    "extract_tool_uses",
    "extract_tool_results",
    "is_error_result",
    "extract_edit_info",
    "extract_bash_info",
    "format_structured_patch",
    # Summarization
    "summarize_action",
    "normalize_tool_name",
    "present_entry",
    # Classifiers
    "classify_claude",
    "classify_gemini",
    "classify_codex",
    "classify_opencode",
    "detect_agent",
    # Index
    "TranscriptIndex",
    "IndexEntry",
    # Exchanges / Parsers
    "get_exchanges",
    "parse_claude_thread",
    "parse_claude_thread_detailed",
    "parse_gemini_thread",
    "parse_codex_thread",
    "get_thread",
    "get_timeline",
    "PARSERS",
    # Formatters
    "format_thread",
    "format_thread_detailed",
    "format_timeline",
    "format_timeline_detailed",
    # Search
    "search_transcripts",
    "_agent_from_path",
    "_get_live_transcript_paths",
    "_get_hcom_tracked_paths",
    "_correlate_paths_to_hcom",
    # Constants
    "TOOL_ALIASES",
    "ERROR_PATTERNS",
]
