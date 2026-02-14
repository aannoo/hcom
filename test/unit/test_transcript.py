"""Unit tests for transcript parsing — self-contained, no real transcripts needed.

Tests pure functions and parsers using synthetic fixtures (tmp_path).
Real-transcript tests live in test/public/real/test_transcript.py.
"""

from hcom.core.transcript import (
    extract_text_content,
    has_user_text,
    extract_files_from_content,
    summarize_action,
    parse_claude_thread,
    parse_claude_thread_detailed,
    parse_gemini_thread,
    parse_codex_thread,
    format_thread,
    format_thread_detailed,
    get_thread,
    is_error_result,
    extract_tool_uses,
    extract_tool_results,
    format_structured_patch,
)


# =============================================================================
# Pure function tests
# =============================================================================


class TestExtractTextContent:
    """Tests for extract_text_content function."""

    def test_string_content(self):
        assert extract_text_content("hello world") == "hello world"
        assert extract_text_content("  trimmed  ") == "trimmed"
        assert extract_text_content("") == ""

    def test_list_content_text_blocks(self):
        content = [
            {"type": "text", "text": "first"},
            {"type": "text", "text": "second"},
        ]
        result = extract_text_content(content)
        assert "first" in result
        assert "second" in result

    def test_list_content_mixed_blocks(self):
        content = [
            {"type": "text", "text": "actual text"},
            {"type": "tool_use", "name": "Bash"},
            {"type": "thinking", "thinking": "internal"},
        ]
        result = extract_text_content(content)
        assert result == "actual text"
        assert "tool_use" not in result
        assert "thinking" not in result

    def test_list_content_tool_result_only(self):
        content = [
            {"type": "tool_result", "tool_use_id": "123", "content": "output"},
        ]
        result = extract_text_content(content)
        assert result == ""

    def test_empty_list(self):
        assert extract_text_content([]) == ""

    def test_non_string_non_list(self):
        assert extract_text_content(None) == ""
        assert extract_text_content(123) == ""
        assert extract_text_content({"key": "value"}) == ""


class TestHasUserText:
    """Tests for has_user_text function."""

    def test_string_content(self):
        assert has_user_text("hello") is True
        assert has_user_text("") is False
        assert has_user_text("   ") is False

    def test_list_with_text(self):
        content = [{"type": "text", "text": "actual prompt"}]
        assert has_user_text(content) is True

    def test_list_tool_result_only(self):
        content = [
            {"type": "tool_result", "tool_use_id": "123", "content": "output"}
        ]
        assert has_user_text(content) is False

    def test_list_mixed_no_real_text(self):
        content = [
            {"type": "tool_result", "content": "output"},
            {"type": "text", "text": ""},  # Empty text
        ]
        assert has_user_text(content) is False


class TestExtractFilesFromContent:
    """Tests for extract_files_from_content function."""

    def test_file_path_extraction(self):
        content = [
            {"type": "tool_use", "input": {"file_path": "/path/to/file.py"}},
        ]
        result = extract_files_from_content(content)
        assert "file.py" in result

    def test_multiple_files(self):
        content = [
            {"type": "tool_use", "input": {"file_path": "/a/b/one.py"}},
            {"type": "tool_use", "input": {"file_path": "/c/d/two.js"}},
            {"type": "tool_use", "input": {"path": "/e/f/three.ts"}},
        ]
        result = extract_files_from_content(content)
        assert "one.py" in result
        assert "two.js" in result
        assert "three.ts" in result

    def test_glob_pattern_extraction(self):
        content = [
            {"type": "tool_use", "input": {"pattern": "src/**/*.py"}},
        ]
        result = extract_files_from_content(content)
        assert "src/" in result

    def test_string_content_returns_empty(self):
        assert extract_files_from_content("hello") == []

    def test_no_file_limit(self):
        """Bug fix #7: Removed internal [:10] cap - callers cap as needed."""
        content = [
            {"type": "tool_use", "input": {"file_path": f"/path/{i}.py"}}
            for i in range(15)
        ]
        result = extract_files_from_content(content)
        assert len(result) == 15


class TestSummarizeAction:
    """Tests for summarize_action function."""

    def test_simple_text(self):
        assert summarize_action("Done editing the file") == "Done editing the file"

    def test_strip_prefixes(self):
        assert "search" in summarize_action("I'll search for the file")
        assert summarize_action("Let me check this").startswith("check")
        assert summarize_action("Sure, I can do that").startswith("I can")

    def test_truncate_long(self):
        long_text = "x" * 300
        result = summarize_action(long_text)
        assert len(result) <= 250  # max_len=200 + suffix
        assert "..." in result

    def test_empty(self):
        assert summarize_action("") == "(no response)"
        assert summarize_action("   \n  \n  ") == "(no response)"


class TestErrorDetection:
    """Tests for error detection functions."""

    def test_is_error_explicit_flag(self):
        result = {"is_error": True, "content": "some output"}
        assert is_error_result(result) is True

    def test_is_error_pattern_rejected(self):
        result = {"is_error": False, "content": "Tool use was rejected by user"}
        assert is_error_result(result) is True

    def test_is_error_pattern_interrupted(self):
        result = {"is_error": False, "content": "Operation was interrupted"}
        assert is_error_result(result) is True

    def test_is_error_pattern_traceback(self):
        result = {"is_error": False, "content": "Traceback (most recent call last):"}
        assert is_error_result(result) is True

    def test_is_error_pattern_failed(self):
        result = {"is_error": False, "content": "FAILED test_foo.py::test_bar"}
        assert is_error_result(result) is True

    def test_not_error_normal_output(self):
        result = {"is_error": False, "content": "All tests passed"}
        assert is_error_result(result) is False


class TestToolExtraction:
    """Tests for tool extraction functions."""

    def test_extract_tool_uses(self):
        content = [
            {"type": "text", "text": "I'll run a command"},
            {"type": "tool_use", "id": "123", "name": "Bash", "input": {"command": "ls"}},
        ]
        tools = extract_tool_uses(content)
        assert len(tools) == 1
        assert tools[0]["name"] == "Bash"
        assert tools[0]["id"] == "123"

    def test_extract_tool_results(self):
        content = [
            {"type": "tool_result", "tool_use_id": "123", "content": "file.txt", "is_error": False},
        ]
        results = extract_tool_results(content)
        assert len(results) == 1
        assert results[0]["tool_use_id"] == "123"
        assert results[0]["is_error"] is False

    def test_extract_from_string_content(self):
        assert extract_tool_uses("hello") == []
        assert extract_tool_results("hello") == []


class TestStructuredPatch:
    """Tests for structured patch formatting."""

    def test_format_simple_patch(self):
        patch = [
            {"oldStart": 10, "newStart": 10, "lines": [" context", "-old", "+new", " more"]}
        ]
        result = format_structured_patch(patch)
        assert "@@ -10 +10 @@" in result
        assert "-old" in result
        assert "+new" in result

    def test_format_empty_patch(self):
        assert format_structured_patch([]) == ""
        assert format_structured_patch(None) == ""

    def test_truncate_long_hunks(self):
        patch = [{"oldStart": 1, "newStart": 1, "lines": [f"line{i}" for i in range(30)]}]
        result = format_structured_patch(patch)
        assert "... +10 more lines" in result


# =============================================================================
# Claude parser tests (synthetic fixtures)
# =============================================================================


class TestParseClaudeThread:
    """Tests for parse_claude_thread on synthetic data."""

    def test_nonexistent_file(self):
        result = parse_claude_thread("/nonexistent/path.jsonl")
        assert result["exchanges"] == []
        assert "not found" in result["error"].lower()

    def test_empty_file(self, tmp_path):
        empty = tmp_path / "empty.jsonl"
        empty.write_text("")
        result = parse_claude_thread(str(empty))
        assert result["exchanges"] == []
        assert result["error"] is None

    def test_malformed_json(self, tmp_path):
        bad = tmp_path / "bad.jsonl"
        bad.write_text("not valid json\n{also bad")
        result = parse_claude_thread(str(bad))
        assert result["exchanges"] == []
        assert result["error"] is None  # Malformed lines skipped, not error

    def test_meta_messages_skipped(self, tmp_path):
        transcript = tmp_path / "meta.jsonl"
        transcript.write_text(
            '{"type":"user","isMeta":true,"message":{"content":"should skip"}}\n'
            '{"type":"user","message":{"content":"actual user message"}}\n'
            '{"type":"assistant","message":{"content":[{"type":"text","text":"response"}]}}\n'
        )
        result = parse_claude_thread(str(transcript))
        # Only non-meta user message should be found
        assert len(result["exchanges"]) == 1
        assert "actual user" in result["exchanges"][0]["user"]


class TestDetailedParser:
    """Tests for parse_claude_thread_detailed."""

    def test_nonexistent_file(self):
        result = parse_claude_thread_detailed("/nonexistent/path.jsonl")
        assert result["exchanges"] == []
        assert result["ended_on_error"] is False
        assert "not found" in result["error"].lower()

    def test_includes_tools_field(self, tmp_path):
        transcript = tmp_path / "test.jsonl"
        transcript.write_text(
            '{"type":"user","message":{"content":"do something"},"uuid":"1","sessionId":"s1","timestamp":"2024-01-01"}\n'
            '{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]},"uuid":"2","sessionId":"s1"}\n'
        )
        result = parse_claude_thread_detailed(str(transcript))
        assert len(result["exchanges"]) == 1
        assert "tools" in result["exchanges"][0]
        assert "edits" in result["exchanges"][0]
        assert "errors" in result["exchanges"][0]
        assert "ended_on_error" in result["exchanges"][0]


# =============================================================================
# Format tests
# =============================================================================


class TestFormatThread:
    """Tests for format_thread function."""

    def test_empty_exchanges(self):
        data = {"exchanges": [], "error": None}
        result = format_thread(data)
        assert "No conversation" in result

    def test_error_formatting(self):
        data = {"exchanges": [], "error": "File not found"}
        result = format_thread(data)
        assert "Error:" in result
        assert "File not found" in result

    def test_with_instance_name(self):
        data = {"exchanges": [{"user": "test", "action": "done", "files": [], "timestamp": ""}]}
        result = format_thread(data, instance="alice")
        assert "@alice" in result

    def test_truncates_long_user(self):
        data = {"exchanges": [{"user": "x" * 400, "action": "done", "files": [], "timestamp": ""}]}
        result = format_thread(data)
        assert "..." in result

    def test_files_displayed(self):
        data = {"exchanges": [{"user": "test", "action": "done", "files": ["a.py", "b.js"], "timestamp": ""}]}
        result = format_thread(data)
        assert "a.py" in result
        assert "b.js" in result


class TestFormatThreadDetailed:
    """Tests for format_thread_detailed."""

    def test_empty_exchanges(self):
        data = {"exchanges": [], "error": None, "ended_on_error": False}
        result = format_thread_detailed(data)
        assert "No conversation" in result

    def test_shows_ended_on_error(self):
        data = {
            "exchanges": [{
                "user": "test",
                "action": "done",
                "files": [],
                "timestamp": "",
                "tools": [],
                "edits": [],
                "errors": [],
                "ended_on_error": True
            }],
            "error": None,
            "ended_on_error": True
        }
        result = format_thread_detailed(data)
        assert "[ENDED ON ERROR]" in result

    def test_shows_tool_errors(self):
        data = {
            "exchanges": [{
                "user": "run tests",
                "action": "running",
                "files": [],
                "timestamp": "",
                "tools": [{"name": "Bash", "command": "pytest", "is_error": True}],
                "edits": [],
                "errors": [{"tool": "Bash", "content": "FAILED"}],
                "ended_on_error": True
            }],
            "error": None,
            "ended_on_error": True
        }
        result = format_thread_detailed(data)
        assert "ERROR" in result
        assert "Bash" in result


# =============================================================================
# get_thread wrapper tests
# =============================================================================


class TestGetThread:
    """Tests for get_thread wrapper function."""

    def test_uses_claude_parser_by_default(self, tmp_path):
        transcript = tmp_path / "test.jsonl"
        transcript.write_text(
            '{"type":"user","message":{"content":"hello"},"uuid":"1"}\n'
            '{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}\n'
        )
        result = get_thread(str(transcript))
        assert len(result["exchanges"]) == 1

    def test_tool_param_defaults_to_claude(self, tmp_path):
        transcript = tmp_path / "test.jsonl"
        transcript.write_text("")

        # Both should work the same
        r1 = get_thread(str(transcript))
        r2 = get_thread(str(transcript), tool="claude")
        assert r1 == r2


class TestGetThreadDetailed:
    """Tests for get_thread with detailed=True."""

    def test_detailed_flag(self, tmp_path):
        transcript = tmp_path / "test.jsonl"
        transcript.write_text(
            '{"type":"user","message":{"content":"hello"},"uuid":"1","sessionId":"s1","timestamp":"2024-01-01"}\n'
            '{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]},"uuid":"2","sessionId":"s1"}\n'
        )
        result = get_thread(str(transcript), detailed=True)
        assert "ended_on_error" in result
        assert "tools" in result["exchanges"][0]


# =============================================================================
# Gemini parser tests (synthetic fixtures)
# =============================================================================


class TestGeminiThreadParsing:
    """Tests for parse_gemini_thread on synthetic data."""

    def test_nonexistent_file(self):
        result = parse_gemini_thread("/nonexistent/path.json")
        assert result["exchanges"] == []
        assert "not found" in result["error"].lower()

    def test_empty_file(self, tmp_path):
        empty = tmp_path / "empty.json"
        empty.write_text("{}")
        result = parse_gemini_thread(str(empty))
        assert result["exchanges"] == []
        assert result["error"] is None

    def test_empty_messages(self, tmp_path):
        empty = tmp_path / "empty.json"
        empty.write_text('{"messages": []}')
        result = parse_gemini_thread(str(empty))
        assert result["exchanges"] == []
        assert result["error"] is None

    def test_basic_conversation(self, tmp_path):
        transcript = tmp_path / "test.json"
        transcript.write_text(
            '{"messages": ['
            '{"id": "1", "type": "user", "content": "hello", "timestamp": "2024-01-01T00:00:00Z"},'
            '{"id": "2", "type": "gemini", "content": "hi there", "timestamp": "2024-01-01T00:00:01Z"}'
            ']}'
        )
        result = parse_gemini_thread(str(transcript))
        assert len(result["exchanges"]) == 1
        assert "hello" in result["exchanges"][0]["user"]
        assert "hi there" in result["exchanges"][0]["action"]

    def test_with_tool_calls(self, tmp_path):
        transcript = tmp_path / "test.json"
        transcript.write_text(
            '{"messages": ['
            '{"id": "1", "type": "user", "content": "run ls", "timestamp": "2024-01-01T00:00:00Z"},'
            '{"id": "2", "type": "gemini", "content": "running command", "toolCalls": [{"name": "run_shell_command", "args": {"command": "ls"}}], "timestamp": "2024-01-01T00:00:01Z"}'
            ']}'
        )
        result = parse_gemini_thread(str(transcript), detailed=True)
        assert len(result["exchanges"]) == 1
        assert "tools" in result["exchanges"][0]
        assert len(result["exchanges"][0]["tools"]) == 1
        assert result["exchanges"][0]["tools"][0]["name"] == "Bash"


# =============================================================================
# Codex parser tests (synthetic fixtures)
# =============================================================================


class TestCodexThreadParsing:
    """Tests for parse_codex_thread on synthetic data."""

    def test_nonexistent_file(self):
        result = parse_codex_thread("/nonexistent/path.jsonl")
        assert result["exchanges"] == []
        assert "not found" in result["error"].lower()

    def test_empty_file(self, tmp_path):
        empty = tmp_path / "empty.jsonl"
        empty.write_text("")
        result = parse_codex_thread(str(empty))
        assert result["exchanges"] == []
        assert result["error"] is None

    def test_basic_conversation(self, tmp_path):
        transcript = tmp_path / "test.jsonl"
        transcript.write_text(
            '{"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"text": "hello"}]}, "timestamp": "2024-01-01T00:00:00Z"}\n'
            '{"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"text": "hi there"}]}, "timestamp": "2024-01-01T00:00:01Z"}\n'
        )
        result = parse_codex_thread(str(transcript))
        assert len(result["exchanges"]) == 1
        assert "hello" in result["exchanges"][0]["user"]
        assert "hi there" in result["exchanges"][0]["action"]

    def test_with_function_calls(self, tmp_path):
        transcript = tmp_path / "test.jsonl"
        transcript.write_text(
            '{"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"text": "run ls"}]}, "timestamp": "2024-01-01T00:00:00Z"}\n'
            '{"type": "response_item", "payload": {"type": "function_call", "name": "shell_command", "arguments": "{\\"command\\": \\"ls\\"}", "call_id": "call_1"}, "timestamp": "2024-01-01T00:00:01Z"}\n'
            '{"type": "response_item", "payload": {"type": "function_call_output", "call_id": "call_1", "output": "Exit code: 0\\nOutput:\\nfile.txt"}, "timestamp": "2024-01-01T00:00:02Z"}\n'
            '{"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"text": "done"}]}, "timestamp": "2024-01-01T00:00:03Z"}\n'
        )
        result = parse_codex_thread(str(transcript), detailed=True)
        assert len(result["exchanges"]) == 1
        assert "tools" in result["exchanges"][0]
        assert len(result["exchanges"][0]["tools"]) == 1
        assert result["exchanges"][0]["tools"][0]["name"] == "Bash"
        assert "ls" in result["exchanges"][0]["tools"][0]["command"]

    def test_error_detection(self, tmp_path):
        transcript = tmp_path / "test.jsonl"
        transcript.write_text(
            '{"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"text": "run bad"}]}, "timestamp": "2024-01-01T00:00:00Z"}\n'
            '{"type": "response_item", "payload": {"type": "function_call", "name": "shell_command", "arguments": "{\\"command\\": \\"bad_command\\"}", "call_id": "call_1"}, "timestamp": "2024-01-01T00:00:01Z"}\n'
            '{"type": "response_item", "payload": {"type": "function_call_output", "call_id": "call_1", "output": "Exit code: 1\\nError: command not found"}, "timestamp": "2024-01-01T00:00:02Z"}\n'
        )
        result = parse_codex_thread(str(transcript), detailed=True)
        assert len(result["exchanges"]) == 1
        assert result["exchanges"][0]["tools"][0]["is_error"] is True


# =============================================================================
# Cross-tool consistency tests (synthetic fixtures)
# =============================================================================


class TestCrossToolConsistency:
    """Test that all three parsers produce consistent output structure."""

    def test_get_thread_routes_correctly(self, tmp_path):
        """get_thread should route to correct parser based on tool param."""
        claude = tmp_path / "claude.jsonl"
        claude.write_text('{"type":"user","message":{"content":"test"},"uuid":"1"}\n')

        gemini = tmp_path / "gemini.json"
        gemini.write_text('{"messages": [{"id": "1", "type": "user", "content": "test"}]}')

        codex = tmp_path / "codex.jsonl"
        codex.write_text(
            '{"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"text": "test"}]}}\n'
        )

        r_claude = get_thread(str(claude), tool="claude")
        r_gemini = get_thread(str(gemini), tool="gemini")
        r_codex = get_thread(str(codex), tool="codex")

        for result in [r_claude, r_gemini, r_codex]:
            assert "exchanges" in result
            assert "total" in result
            assert "error" in result

    def test_detailed_mode_consistency(self, tmp_path):
        """Detailed mode should produce consistent structure for all tools."""
        claude = tmp_path / "claude.jsonl"
        claude.write_text(
            '{"type":"user","message":{"content":"test"},"uuid":"1","sessionId":"s1","timestamp":"2024-01-01"}\n'
            '{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]},"uuid":"2","sessionId":"s1"}\n'
        )

        gemini = tmp_path / "gemini.json"
        gemini.write_text(
            '{"messages": ['
            '{"id": "1", "type": "user", "content": "test", "timestamp": "2024-01-01"},'
            '{"id": "2", "type": "gemini", "content": "done", "toolCalls": [{"name": "run_shell_command", "args": {"command": "ls"}}]}'
            ']}'
        )

        codex = tmp_path / "codex.jsonl"
        codex.write_text(
            '{"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"text": "test"}]}, "timestamp": "2024-01-01"}\n'
            '{"type": "response_item", "payload": {"type": "function_call", "name": "shell_command", "arguments": "{\\"command\\": \\"ls\\"}", "call_id": "c1"}, "timestamp": "2024-01-01"}\n'
            '{"type": "response_item", "payload": {"type": "function_call_output", "call_id": "c1", "output": "Exit code: 0"}, "timestamp": "2024-01-01"}\n'
            '{"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"text": "done"}]}, "timestamp": "2024-01-01"}\n'
        )

        r_claude = get_thread(str(claude), tool="claude", detailed=True)
        r_gemini = get_thread(str(gemini), tool="gemini", detailed=True)
        r_codex = get_thread(str(codex), tool="codex", detailed=True)

        for result, name in [(r_claude, "claude"), (r_gemini, "gemini"), (r_codex, "codex")]:
            assert result["error"] is None, f"{name} had error: {result['error']}"
            if result["exchanges"]:
                ex = result["exchanges"][0]
                assert "tools" in ex, f"{name} missing tools field"
                assert isinstance(ex["tools"], list), f"{name} tools not a list"

    def test_tool_name_normalization(self, tmp_path):
        """Tool names should be normalized consistently across tools."""
        gemini = tmp_path / "gemini.json"
        gemini.write_text(
            '{"messages": ['
            '{"id": "1", "type": "user", "content": "test"},'
            '{"id": "2", "type": "gemini", "content": "done", "toolCalls": [{"name": "run_shell_command", "args": {"command": "ls"}}]}'
            ']}'
        )

        codex = tmp_path / "codex.jsonl"
        codex.write_text(
            '{"type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"text": "test"}]}}\n'
            '{"type": "response_item", "payload": {"type": "function_call", "name": "shell_command", "arguments": "{\\"command\\": \\"ls\\"}", "call_id": "c1"}}\n'
            '{"type": "response_item", "payload": {"type": "function_call_output", "call_id": "c1", "output": "Exit code: 0"}}\n'
        )

        r_gemini = get_thread(str(gemini), tool="gemini", detailed=True)
        r_codex = get_thread(str(codex), tool="codex", detailed=True)

        gemini_tool = r_gemini["exchanges"][0]["tools"][0]["name"]
        codex_tool = r_codex["exchanges"][0]["tools"][0]["name"]

        assert gemini_tool == "Bash", f"Gemini tool not normalized: {gemini_tool}"
        assert codex_tool == "Bash", f"Codex tool not normalized: {codex_tool}"
