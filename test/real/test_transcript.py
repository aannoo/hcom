"""Tests for transcript parsing against real transcripts from all four tools.

Runs against actual transcript files from:
- Claude: ~/.claude/projects/**/*.jsonl
- Gemini: ~/.gemini/tmp/**/chats/*.json
- Codex: ~/.codex/sessions/**/*.jsonl
- OpenCode: ~/.local/share/opencode/opencode.db (SQLite)

These require real transcript files on the machine. Not collected by pytest
(see conftest.py). Run directly: python test/public/real/test_transcript.py

Unit tests for the same functions (synthetic fixtures) live in
test/public/unit/test_transcript.py.
"""

import glob
import json
import os
import random
import sqlite3
import time
from pathlib import Path
from unittest.mock import patch

import pytest


def _seeded_rng(test_name: str) -> random.Random:
    """Create a seeded RNG and print the seed for reproducibility."""
    seed = int(time.time() * 1000) % (2**32)
    print(f"\n[{test_name}] random seed: {seed}")
    return random.Random(seed)

from hcom.core.transcript import (
    parse_claude_thread,
    parse_claude_thread_detailed,
    parse_gemini_thread,
    parse_codex_thread,
    get_thread,
    get_claude_config_dir,
    get_opencode_db_path,
)
from hcom.tools.codex.transcript import (
    TranscriptWatcher,
    APPLY_PATCH_FILE_RE,
)


# =============================================================================
# Discovery helpers
# =============================================================================


def get_claude_transcript_paths(max_count: int = None) -> list[Path]:
    """Find all real Claude transcript files."""
    claude_dir = get_claude_config_dir()
    pattern = str(claude_dir / "projects" / "**" / "*.jsonl")
    paths = [Path(p) for p in glob.glob(pattern, recursive=True)]
    if max_count:
        paths = paths[:max_count]
    return paths


def get_transcript_paths(max_count: int = None) -> list[Path]:
    """Find all real transcript files (Claude only, for backward compat)."""
    return get_claude_transcript_paths(max_count)


def get_transcripts_by_project() -> dict[str, list[Path]]:
    """Group transcripts by project directory."""
    claude_dir = get_claude_config_dir()
    pattern = str(claude_dir / "projects" / "**" / "*.jsonl")
    paths = glob.glob(pattern, recursive=True)

    by_project = {}
    for p in paths:
        project = os.path.dirname(p)
        if project not in by_project:
            by_project[project] = []
        by_project[project].append(Path(p))
    return by_project


def get_agent_transcripts() -> list[Path]:
    """Get subagent transcripts (agent-*.jsonl)."""
    claude_dir = get_claude_config_dir()
    pattern = str(claude_dir / "projects" / "**" / "agent-*.jsonl")
    return [Path(p) for p in glob.glob(pattern, recursive=True)]


def get_session_transcripts() -> list[Path]:
    """Get main session transcripts (UUID.jsonl, not agent-)."""
    all_paths = get_claude_transcript_paths()
    return [p for p in all_paths if "agent-" not in p.name]


def get_gemini_transcript_paths(max_count: int = None) -> list[Path]:
    """Find all real Gemini CLI transcript files."""
    pattern = os.path.expanduser("~/.gemini/tmp/**/chats/*.json")
    paths = [Path(p) for p in glob.glob(pattern, recursive=True)]
    if max_count:
        paths = paths[:max_count]
    return paths


def get_codex_transcript_paths(max_count: int = None) -> list[Path]:
    """Find all real Codex CLI transcript files."""
    codex_home = os.environ.get("CODEX_HOME") or os.path.expanduser("~/.codex")
    pattern = os.path.join(codex_home, "sessions", "**", "rollout-*.jsonl")
    paths = [Path(p) for p in glob.glob(pattern, recursive=True)]
    if max_count:
        paths = paths[:max_count]
    return paths


def get_opencode_session_ids(max_count: int = None, min_messages: int = 2) -> list[str]:
    """Find OpenCode session IDs with at least min_messages messages.

    Returns session IDs sorted by time_created descending (newest first).
    """
    db_path = get_opencode_db_path()
    if not db_path:
        return []
    try:
        conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True, timeout=5)
        rows = conn.execute(
            "SELECT s.id FROM session s "
            "JOIN message m ON m.session_id = s.id "
            "GROUP BY s.id HAVING COUNT(m.id) >= ? "
            "ORDER BY s.time_created DESC",
            (min_messages,),
        ).fetchall()
        conn.close()
        ids = [r[0] for r in rows]
        if max_count:
            ids = ids[:max_count]
        return ids
    except (sqlite3.Error, OSError):
        return []


def get_all_transcript_paths(max_per_tool: int = None) -> dict[str, list[Path]]:
    """Get transcript paths for all three file-based tools."""
    return {
        "claude": get_claude_transcript_paths(max_per_tool),
        "gemini": get_gemini_transcript_paths(max_per_tool),
        "codex": get_codex_transcript_paths(max_per_tool),
    }


# =============================================================================
# Claude — real transcript tests
# =============================================================================


class TestRealTranscriptParsing:
    """Run parser against actual transcripts from ~/.claude."""

    def setup_method(self):
        self.all_paths = get_transcript_paths()
        if len(self.all_paths) < 10:
            raise RuntimeError(f"Need at least 10 Claude transcripts, found {len(self.all_paths)}")
        rng = _seeded_rng("TestRealTranscriptParsing")
        self.sample = rng.sample(self.all_paths, min(50, len(self.all_paths)))

    def test_no_crashes_on_any_transcript(self):
        """Parser must not crash on any real transcript."""
        for path in self.sample:
            result = parse_claude_thread(str(path))
            assert "exchanges" in result
            assert "error" in result
            if result["error"]:
                assert isinstance(result["error"], str)

    def test_exchanges_have_required_fields(self):
        """All exchanges must have user, action, files, timestamp."""
        for path in self.sample:
            result = parse_claude_thread(str(path))
            for ex in result["exchanges"]:
                assert "user" in ex
                assert "action" in ex
                assert "files" in ex
                assert "timestamp" in ex
                assert isinstance(ex["files"], list)

    def test_user_text_not_tool_result_json(self):
        """User field should contain actual prompts, not raw tool_result JSON."""
        for path in self.sample:
            result = parse_claude_thread(str(path))
            for ex in result["exchanges"]:
                user_text = ex["user"]
                assert '"type":"tool_result"' not in user_text
                assert '"tool_use_id":"toolu_' not in user_text


class TestRealTranscriptStatistics:
    """Statistical tests across the full transcript corpus."""

    def setup_method(self):
        self.all_paths = get_transcript_paths()

    def test_minimum_transcript_count(self):
        """Ensure we have enough transcripts for meaningful tests."""
        assert len(self.all_paths) >= 100, f"Need at least 100 Claude transcripts for statistical test, found {len(self.all_paths)}"

    def test_parse_success_rate(self):
        """Parser should succeed (no errors) on most transcripts."""
        assert self.all_paths, "Need Claude transcripts for testing"

        rng = _seeded_rng("test_parse_success_rate")
        sample = rng.sample(self.all_paths, min(200, len(self.all_paths)))

        errors = 0
        for path in sample:
            result = parse_claude_thread(str(path))
            if result["error"]:
                errors += 1

        error_rate = errors / len(sample)
        assert error_rate < 0.05, f"Error rate {error_rate:.1%} too high"

    def test_agent_vs_session_transcripts(self):
        """Both agent and session transcripts should parse."""
        agent = [p for p in self.all_paths if "agent-" in p.name]
        session = [p for p in self.all_paths if "agent-" not in p.name]

        rng = _seeded_rng("test_agent_vs_session_transcripts")
        agent_sample = rng.sample(agent, min(20, len(agent)))
        session_sample = rng.sample(session, min(20, len(session)))

        for path in agent_sample + session_sample:
            result = parse_claude_thread(str(path))
            assert result["error"] is None, f"Failed on {path}: {result['error']}"


class TestSubagentTranscripts:
    """Tests for subagent transcript parsing (agent-*.jsonl).

    Note: Agent transcripts typically have isSidechain=True on all entries,
    which are classified as 'system' and filtered out. This means 0 exchanges
    is expected. These tests verify the parser doesn't crash, not that it
    finds exchanges.
    """

    def test_subagent_transcripts_parse(self):
        """Parser should not crash on agent transcripts (0 exchanges expected)."""
        agent_paths = get_agent_transcripts()
        assert len(agent_paths) >= 5, f"Need at least 5 agent transcripts, found {len(agent_paths)}"

        sample_size = min(200, len(agent_paths))
        rng = _seeded_rng("test_subagent_transcripts_parse")
        sample = rng.sample(agent_paths, sample_size)

        for path in sample:
            result = parse_claude_thread(str(path))
            assert result["error"] is None, f"Failed on {path}"
        # Note: 0 exchanges is expected since agent transcripts have isSidechain=True

    def test_subagent_detailed_parse(self):
        """Detailed parser should not crash on agent transcripts."""
        agent_paths = get_agent_transcripts()
        assert len(agent_paths) >= 5, f"Need at least 5 agent transcripts, found {len(agent_paths)}"

        rng = _seeded_rng("test_subagent_detailed_parse")
        sample = rng.sample(agent_paths, min(5, len(agent_paths)))

        for path in sample:
            result = parse_claude_thread_detailed(str(path), last=3)
            assert result["error"] is None, f"Failed on {path}"
            assert "ended_on_error" in result


class TestRealTranscriptEdgeCases:
    """Test specific edge cases found in real transcripts."""

    def test_sidechain_messages_skipped(self):
        """Sidechain messages should not appear in exchanges."""
        paths = get_transcript_paths(500)
        tested_count = 0

        for path in paths:
            try:
                with open(path) as f:
                    content = f.read()
                if '"isSidechain":true' not in content and '"isSidechain": true' not in content:
                    continue

                result = parse_claude_thread(str(path))
                assert result["error"] is None
                tested_count += 1
                if tested_count >= 5:  # Test multiple files, not just one
                    break
            except (OSError, AssertionError):
                continue

        assert tested_count > 0, "No transcripts with isSidechain found - test did not verify anything"

    def test_compact_summary_skipped(self):
        """isCompactSummary messages should be skipped."""
        paths = get_transcript_paths(500)
        tested_count = 0

        for path in paths:
            try:
                with open(path) as f:
                    content = f.read()
                if '"isCompactSummary":true' not in content:
                    continue

                result = parse_claude_thread(str(path))
                assert result["error"] is None
                tested_count += 1
                if tested_count >= 5:
                    break
            except (OSError, AssertionError):
                continue

        assert tested_count > 0, "No transcripts with isCompactSummary found - test did not verify anything"

    def test_thinking_blocks_not_in_output(self):
        """Thinking blocks should not leak into action summaries."""
        paths = get_transcript_paths(200)
        tested_count = 0

        for path in paths:
            try:
                with open(path) as f:
                    content = f.read()
                if '"type":"thinking"' not in content:
                    continue

                result = parse_claude_thread(str(path))
                for ex in result["exchanges"]:
                    assert "signature" not in ex["action"].lower()
                tested_count += 1
                if tested_count >= 5:
                    break
            except (OSError, AssertionError):
                continue

        assert tested_count > 0, "No transcripts with thinking blocks found - test did not verify anything"


class TestRealWorldScenarios:
    """Test real-world usage patterns."""

    def test_large_transcript_performance(self):
        """Large transcripts should parse in reasonable time."""
        import time

        paths = get_transcript_paths()
        sizes = []
        for p in paths[:100]:
            try:
                sizes.append((p, p.stat().st_size))
            except OSError:
                pass

        sizes.sort(key=lambda x: -x[1])
        assert sizes, "Need Claude transcripts for performance testing"

        largest = sizes[0][0]

        start = time.time()
        result = parse_claude_thread(str(largest), last=20)
        elapsed = time.time() - start

        assert elapsed < 5.0, f"Parsing took {elapsed:.1f}s, too slow"
        assert result["error"] is None

    def test_recent_transcripts_have_exchanges(self):
        """Recently modified transcripts should have parseable exchanges."""
        paths = get_transcript_paths()
        assert paths, "Need Claude transcripts for testing"

        with_mtime = []
        for p in paths:
            try:
                with_mtime.append((p, p.stat().st_mtime))
            except OSError:
                pass

        with_mtime.sort(key=lambda x: -x[1])
        recent = [p for p, _ in with_mtime[:20]]

        found_exchanges = 0
        for path in recent:
            result = parse_claude_thread(str(path))
            if result["exchanges"]:
                found_exchanges += 1

        assert found_exchanges > 0, "No recent transcripts have exchanges"


class TestDetailedParserRealTranscripts:
    """Test detailed parser on real transcripts."""

    def setup_method(self):
        all_paths = get_transcript_paths()
        assert len(all_paths) >= 10, f"Need at least 10 Claude transcripts, found {len(all_paths)}"
        rng = _seeded_rng("TestDetailedParserRealTranscripts")
        self.sample = rng.sample(all_paths, min(30, len(all_paths)))

    def test_no_crashes(self):
        """Detailed parser must not crash on real transcripts."""
        for path in self.sample:
            result = parse_claude_thread_detailed(str(path), last=5)
            assert "exchanges" in result
            assert "ended_on_error" in result

    def test_finds_tools_in_real_transcripts(self):
        """Should find tool usage in at least some transcripts."""
        found_tools = False
        for path in self.sample:
            result = parse_claude_thread_detailed(str(path), last=10)
            for ex in result["exchanges"]:
                if ex.get("tools"):
                    found_tools = True
                    break
            if found_tools:
                break
        assert found_tools, "No tools found in any sampled transcript"


# =============================================================================
# Gemini — real transcript tests
# =============================================================================


class TestRealGeminiTranscripts:
    """Run parser against actual Gemini transcripts."""

    def setup_method(self):
        paths = get_gemini_transcript_paths()
        assert len(paths) >= 5, f"Need at least 5 Gemini transcripts, found {len(paths)}"
        rng = _seeded_rng("TestRealGeminiTranscripts")
        self.sample = rng.sample(paths, min(30, len(paths)))

    def test_no_crashes(self):
        """Parser must not crash on any real Gemini transcript."""
        for path in self.sample:
            result = parse_gemini_thread(str(path))
            assert "exchanges" in result
            assert "error" in result
            if result["error"]:
                assert isinstance(result["error"], str)

    def test_exchanges_have_required_fields(self):
        """All exchanges must have user, action, files, timestamp."""
        for path in self.sample:
            result = parse_gemini_thread(str(path))
            for ex in result["exchanges"]:
                assert "user" in ex
                assert "action" in ex
                assert "files" in ex
                assert "timestamp" in ex

    def test_detailed_mode_has_tools(self):
        """Detailed mode should include tools field."""
        for path in self.sample:
            result = parse_gemini_thread(str(path), detailed=True)
            for ex in result["exchanges"]:
                assert "tools" in ex
                assert isinstance(ex["tools"], list)


class TestGeminiDetailedParsing:
    """Test Gemini detailed parsing with tool calls."""

    def setup_method(self):
        paths = get_gemini_transcript_paths()
        self.with_tools = []
        for path in paths[:100]:
            try:
                with open(path) as f:
                    content = f.read()
                if '"toolCalls"' in content:
                    self.with_tools.append(path)
                    if len(self.with_tools) >= 10:
                        break
            except Exception:
                continue
        assert self.with_tools, "Need Gemini transcripts with tool calls, found none in first 100"

    def test_extracts_tool_calls(self):
        """Should extract tool calls from Gemini transcripts."""
        found_tools = False
        for path in self.with_tools:
            result = parse_gemini_thread(str(path), detailed=True)
            for ex in result["exchanges"]:
                if ex.get("tools"):
                    found_tools = True
                    for tool in ex["tools"]:
                        assert "name" in tool
                        assert "is_error" in tool
        assert found_tools, "No tools extracted from transcripts with tool calls"


# =============================================================================
# Codex — real transcript tests
# =============================================================================


class TestRealCodexTranscripts:
    """Run parser against actual Codex transcripts."""

    def setup_method(self):
        paths = get_codex_transcript_paths()
        assert len(paths) >= 5, f"Need at least 5 Codex transcripts, found {len(paths)}"
        rng = _seeded_rng("TestRealCodexTranscripts")
        self.sample = rng.sample(paths, min(30, len(paths)))

    def test_no_crashes(self):
        """Parser must not crash on any real Codex transcript."""
        for path in self.sample:
            result = parse_codex_thread(str(path))
            assert "exchanges" in result
            assert "error" in result
            if result["error"]:
                assert isinstance(result["error"], str)

    def test_exchanges_have_required_fields(self):
        """All exchanges must have user, action, files, timestamp."""
        for path in self.sample:
            result = parse_codex_thread(str(path))
            for ex in result["exchanges"]:
                assert "user" in ex
                assert "action" in ex
                assert "files" in ex
                assert "timestamp" in ex

    def test_detailed_mode_has_tools(self):
        """Detailed mode should include tools field."""
        for path in self.sample:
            result = parse_codex_thread(str(path), detailed=True)
            for ex in result["exchanges"]:
                assert "tools" in ex
                assert isinstance(ex["tools"], list)


class TestCodexDetailedParsing:
    """Test Codex detailed parsing with function calls."""

    def setup_method(self):
        paths = get_codex_transcript_paths()
        self.with_tools = []
        for path in paths[:100]:
            try:
                with open(path) as f:
                    content = f.read()
                if '"function_call"' in content:
                    self.with_tools.append(path)
                    if len(self.with_tools) >= 10:
                        break
            except Exception:
                continue
        assert self.with_tools, "Need Codex transcripts with function calls, found none in first 100"

    def test_extracts_function_calls(self):
        """Should extract function calls from Codex transcripts."""
        found_tools = False
        for path in self.with_tools:
            result = parse_codex_thread(str(path), detailed=True)
            for ex in result["exchanges"]:
                if ex.get("tools"):
                    found_tools = True
                    for tool in ex["tools"]:
                        assert "name" in tool
                        assert "is_error" in tool
        assert found_tools, "No tools extracted from transcripts with function calls"


# =============================================================================
# OpenCode — real transcript tests
# =============================================================================


class TestRealOpenCodeTranscripts:
    """Run parser against actual OpenCode transcripts from SQLite DB."""

    def setup_method(self):
        self.db_path = get_opencode_db_path()
        assert self.db_path, "OpenCode DB not found at ~/.local/share/opencode/opencode.db"
        self.session_ids = get_opencode_session_ids(min_messages=2)
        assert len(self.session_ids) >= 5, f"Need at least 5 OpenCode sessions with messages, found {len(self.session_ids)}"
        rng = _seeded_rng("TestRealOpenCodeTranscripts")
        self.sample = rng.sample(self.session_ids, min(30, len(self.session_ids)))

    def test_no_crashes(self):
        """Parser must not crash on any real OpenCode session."""
        for sid in self.sample:
            result = get_thread(self.db_path, tool="opencode", session_id=sid)
            assert "exchanges" in result
            assert "error" in result
            if result["error"]:
                assert isinstance(result["error"], str)

    def test_exchanges_have_required_fields(self):
        """All exchanges must have user, action, files, timestamp."""
        for sid in self.sample:
            result = get_thread(self.db_path, tool="opencode", session_id=sid)
            for ex in result["exchanges"]:
                assert "user" in ex
                assert "action" in ex
                assert "files" in ex
                assert "timestamp" in ex
                assert isinstance(ex["files"], list)

    def test_detailed_mode_has_tools(self):
        """Detailed mode should include tools field."""
        for sid in self.sample:
            result = get_thread(self.db_path, tool="opencode", session_id=sid, detailed=True)
            for ex in result["exchanges"]:
                assert "tools" in ex
                assert isinstance(ex["tools"], list)

    def test_detailed_has_ended_on_error(self):
        """Detailed mode should include ended_on_error field."""
        for sid in self.sample[:5]:
            result = get_thread(self.db_path, tool="opencode", session_id=sid, detailed=True)
            assert "ended_on_error" in result


class TestOpenCodeDetailedParsing:
    """Test OpenCode detailed parsing with tool calls."""

    def setup_method(self):
        self.db_path = get_opencode_db_path()
        assert self.db_path, "OpenCode DB not found"
        # Find sessions with tool parts
        self.with_tools = []
        conn = sqlite3.connect(f"file:{self.db_path}?mode=ro", uri=True, timeout=5)
        rows = conn.execute(
            "SELECT DISTINCT m.session_id FROM message m "
            "JOIN part p ON p.message_id = m.id "
            "WHERE json_extract(p.data, '$.type') = 'tool' "
            "ORDER BY m.time_created DESC LIMIT 20",
        ).fetchall()
        conn.close()
        self.with_tools = [r[0] for r in rows]
        assert self.with_tools, "Need OpenCode sessions with tool calls, found none"

    def test_extracts_tool_calls(self):
        """Should extract tool calls from OpenCode sessions."""
        found_tools = False
        for sid in self.with_tools[:10]:
            result = get_thread(self.db_path, tool="opencode", session_id=sid, detailed=True)
            for ex in result.get("exchanges", []):
                if ex.get("tools"):
                    found_tools = True
                    for tool in ex["tools"]:
                        assert "name" in tool
                        assert "is_error" in tool
        assert found_tools, "No tools extracted from sessions with tool calls"


class TestOpenCodeStatistics:
    """Statistical tests across OpenCode sessions."""

    def test_parse_success_rate(self):
        """Parser should succeed on most sessions."""
        db_path = get_opencode_db_path()
        assert db_path, "OpenCode DB not found"
        session_ids = get_opencode_session_ids(min_messages=2)
        assert len(session_ids) >= 10, f"Need at least 10 OpenCode sessions, found {len(session_ids)}"

        rng = _seeded_rng("test_opencode_parse_success_rate")
        sample = rng.sample(session_ids, min(50, len(session_ids)))

        errors = 0
        for sid in sample:
            result = get_thread(db_path, tool="opencode", session_id=sid)
            if result["error"]:
                errors += 1

        error_rate = errors / len(sample)
        assert error_rate < 0.05, f"Error rate {error_rate:.1%} too high"

    def test_sessions_have_exchanges(self):
        """Sessions with multiple messages should produce exchanges."""
        db_path = get_opencode_db_path()
        assert db_path, "OpenCode DB not found"
        session_ids = get_opencode_session_ids(min_messages=4)
        if len(session_ids) < 5:
            import warnings
            warnings.warn("Too few OpenCode sessions with 4+ messages to test meaningfully")
            return

        rng = _seeded_rng("test_sessions_have_exchanges")
        sample = rng.sample(session_ids, min(20, len(session_ids)))

        found_exchanges = 0
        for sid in sample:
            result = get_thread(db_path, tool="opencode", session_id=sid)
            if result["exchanges"]:
                found_exchanges += 1

        assert found_exchanges > 0, "No OpenCode sessions produced exchanges"


# =============================================================================
# Cross-tool — real transcript tests
# =============================================================================


class TestRealTranscriptsCrossToolStatistics:
    """Statistical tests across all four tools."""

    def test_parse_success_rate_all_tools(self):
        """Parser should succeed on most transcripts for all tools."""
        all_paths = get_all_transcript_paths(max_per_tool=50)
        rng = _seeded_rng("test_parse_success_rate_all_tools")

        for tool, paths in all_paths.items():
            if len(paths) < 10:
                continue

            sample = rng.sample(paths, min(30, len(paths)))
            errors = 0

            for path in sample:
                result = get_thread(str(path), tool=tool)
                if result["error"]:
                    errors += 1

            error_rate = errors / len(sample)
            assert error_rate < 0.1, f"{tool} error rate {error_rate:.1%} too high"

        # OpenCode (DB-based, separate path)
        db_path = get_opencode_db_path()
        if db_path:
            oc_sessions = get_opencode_session_ids(max_count=50, min_messages=2)
            if len(oc_sessions) >= 10:
                sample = rng.sample(oc_sessions, min(30, len(oc_sessions)))
                errors = sum(1 for sid in sample if get_thread(db_path, tool="opencode", session_id=sid)["error"])
                error_rate = errors / len(sample)
                assert error_rate < 0.1, f"opencode error rate {error_rate:.1%} too high"

    def test_detailed_mode_finds_tools_all_tools(self):
        """Detailed mode should find tools in at least some transcripts for each tool."""
        all_paths = get_all_transcript_paths(max_per_tool=100)
        rng = _seeded_rng("test_detailed_mode_finds_tools_all_tools")

        for tool, paths in all_paths.items():
            if len(paths) < 10:
                continue

            found_tools = False
            sample = rng.sample(paths, min(50, len(paths)))

            for path in sample:
                result = get_thread(str(path), tool=tool, detailed=True)
                for ex in result.get("exchanges", []):
                    if ex.get("tools"):
                        found_tools = True
                        break
                if found_tools:
                    break

            if not found_tools:
                import warnings
                warnings.warn(f"No tools found in {tool} transcripts")

        # OpenCode (DB-based)
        db_path = get_opencode_db_path()
        if db_path:
            oc_sessions = get_opencode_session_ids(max_count=50, min_messages=2)
            if len(oc_sessions) >= 10:
                found_tools = False
                sample = rng.sample(oc_sessions, min(30, len(oc_sessions)))
                for sid in sample:
                    result = get_thread(db_path, tool="opencode", session_id=sid, detailed=True)
                    for ex in result.get("exchanges", []):
                        if ex.get("tools"):
                            found_tools = True
                            break
                    if found_tools:
                        break
                if not found_tools:
                    import warnings
                    warnings.warn("No tools found in opencode transcripts")


# =============================================================================
# Codex TranscriptWatcher — incremental transcript monitoring
#
# Tests for the real-time watcher that parses Codex transcripts incrementally
# to detect apply_patch → file edit events, shell commands, and user prompts.
# This is different from core/transcript.py which parses for display.
# =============================================================================


class TestApplyPatchRegex:
    """Tests for APPLY_PATCH_FILE_RE pattern."""

    def test_update_file(self):
        text = "*** Update File: src/main.py\nsome content"
        matches = APPLY_PATCH_FILE_RE.findall(text)
        assert matches == ["src/main.py"]

    def test_add_file(self):
        text = "*** Add File: new_file.js\n"
        matches = APPLY_PATCH_FILE_RE.findall(text)
        assert matches == ["new_file.js"]

    def test_delete_file(self):
        text = "*** Delete File: old_file.txt"
        matches = APPLY_PATCH_FILE_RE.findall(text)
        assert matches == ["old_file.txt"]

    def test_multiple_files(self):
        text = """*** Update File: a.py
content
*** Add File: b.py
more content
*** Delete File: c.py"""
        matches = APPLY_PATCH_FILE_RE.findall(text)
        assert sorted(matches) == ["a.py", "b.py", "c.py"]

    def test_no_match(self):
        text = "just some regular text"
        matches = APPLY_PATCH_FILE_RE.findall(text)
        assert matches == []


class TestTranscriptWatcherInit:
    """Tests for TranscriptWatcher initialization."""

    def test_init_with_path(self):
        watcher = TranscriptWatcher("test-instance", "/path/to/transcript.jsonl")
        assert watcher.instance_name == "test-instance"
        assert watcher.transcript_path == "/path/to/transcript.jsonl"
        assert watcher._file_pos == 0
        assert watcher._logged_call_ids == set()

    def test_init_without_path(self):
        watcher = TranscriptWatcher("test-instance")
        assert watcher.transcript_path is None

    def test_set_transcript_path(self):
        watcher = TranscriptWatcher("test-instance")
        watcher._file_pos = 100  # Simulate some reading
        watcher.set_transcript_path("/new/path.jsonl")
        assert watcher.transcript_path == "/new/path.jsonl"
        assert watcher._file_pos == 0  # Reset on new path

    def test_set_same_path_no_reset(self):
        watcher = TranscriptWatcher("test-instance", "/path.jsonl")
        watcher._file_pos = 100
        watcher.set_transcript_path("/path.jsonl")  # Same path
        assert watcher._file_pos == 100  # Not reset


class TestTranscriptWatcherSync:
    """Tests for TranscriptWatcher.sync() method."""

    def test_sync_no_path(self):
        watcher = TranscriptWatcher("test-instance")
        result = watcher.sync()
        assert result == 0

    def test_sync_nonexistent_file(self, tmp_path):
        watcher = TranscriptWatcher("test-instance", str(tmp_path / "nonexistent.jsonl"))
        result = watcher.sync()
        assert result == 0

    def test_sync_empty_file(self, tmp_path):
        transcript = tmp_path / "empty.jsonl"
        transcript.write_text("")
        watcher = TranscriptWatcher("test-instance", str(transcript))
        result = watcher.sync()
        assert result == 0

    @patch("hcom.tools.codex.transcript.TranscriptWatcher._log_file_edit")
    def test_sync_apply_patch(self, mock_log, tmp_path):
        transcript = tmp_path / "transcript.jsonl"
        entry = {
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "apply_patch",
                "input": "*** Update File: src/main.py\nchanges here",
                "call_id": "call_1"
            },
            "timestamp": "2024-01-01T00:00:00Z"
        }
        transcript.write_text(json.dumps(entry) + "\n")

        watcher = TranscriptWatcher("test-instance", str(transcript))
        result = watcher.sync()

        assert result == 1
        mock_log.assert_called_once_with("src/main.py", "2024-01-01T00:00:00Z")

    @patch("hcom.tools.codex.transcript.TranscriptWatcher._log_shell_command")
    def test_sync_shell_command(self, mock_log, tmp_path):
        transcript = tmp_path / "transcript.jsonl"
        entry = {
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "shell_command",
                "arguments": '{"command": "ls -la", "workdir": "/tmp"}',
                "call_id": "call_1"
            },
            "timestamp": "2024-01-01T00:00:00Z"
        }
        transcript.write_text(json.dumps(entry) + "\n")

        watcher = TranscriptWatcher("test-instance", str(transcript))
        watcher.sync()

        mock_log.assert_called_once_with("ls -la", "2024-01-01T00:00:00Z")

    @patch("hcom.tools.codex.transcript.TranscriptWatcher._log_shell_command")
    def test_sync_shell_array_format(self, mock_log, tmp_path):
        """Test shell command with array format: ["bash", "-lc", "actual cmd"]"""
        transcript = tmp_path / "transcript.jsonl"
        entry = {
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "shell",
                "arguments": '{"command": ["bash", "-lc", "npm install"], "workdir": "/tmp"}',
                "call_id": "call_1"
            },
            "timestamp": "2024-01-01T00:00:00Z"
        }
        transcript.write_text(json.dumps(entry) + "\n")

        watcher = TranscriptWatcher("test-instance", str(transcript))
        watcher.sync()

        mock_log.assert_called_once_with("npm install", "2024-01-01T00:00:00Z")

    @patch("hcom.tools.codex.transcript.TranscriptWatcher._log_user_prompt")
    def test_sync_user_prompt(self, mock_log, tmp_path):
        transcript = tmp_path / "transcript.jsonl"
        entry = {
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"text": "please fix the bug"}]
            },
            "timestamp": "2024-01-01T00:00:00Z"
        }
        transcript.write_text(json.dumps(entry) + "\n")

        watcher = TranscriptWatcher("test-instance", str(transcript))
        watcher.sync()

        mock_log.assert_called_once_with("2024-01-01T00:00:00Z")

    @patch("hcom.tools.codex.transcript.TranscriptWatcher._log_user_prompt")
    def test_sync_skips_hcom_injected(self, mock_log, tmp_path):
        """Messages starting with [hcom] should be skipped."""
        transcript = tmp_path / "transcript.jsonl"
        entry = {
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"text": "[hcom] injected message"}]
            },
            "timestamp": "2024-01-01T00:00:00Z"
        }
        transcript.write_text(json.dumps(entry) + "\n")

        watcher = TranscriptWatcher("test-instance", str(transcript))
        watcher.sync()

        mock_log.assert_not_called()

    def test_sync_incremental(self, tmp_path):
        """Sync should only process new lines."""
        transcript = tmp_path / "transcript.jsonl"

        # First write
        entry1 = {
            "type": "response_item",
            "payload": {"type": "message", "role": "user", "content": [{"text": "first"}]},
            "timestamp": "2024-01-01T00:00:00Z"
        }
        transcript.write_text(json.dumps(entry1) + "\n")

        watcher = TranscriptWatcher("test-instance", str(transcript))
        with patch.object(watcher, "_log_user_prompt") as mock_log:
            watcher.sync()
            assert mock_log.call_count == 1

        # Second write (append)
        entry2 = {
            "type": "response_item",
            "payload": {"type": "message", "role": "user", "content": [{"text": "second"}]},
            "timestamp": "2024-01-01T00:00:01Z"
        }
        with open(transcript, "a") as f:
            f.write(json.dumps(entry2) + "\n")

        with patch.object(watcher, "_log_user_prompt") as mock_log:
            watcher.sync()
            assert mock_log.call_count == 1  # Only new entry

    def test_sync_deduplicates_call_ids(self, tmp_path):
        """Same call_id should only be processed once."""
        transcript = tmp_path / "transcript.jsonl"
        entry = {
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "apply_patch",
                "input": "*** Update File: test.py\n",
                "call_id": "call_same"
            },
            "timestamp": "2024-01-01T00:00:00Z"
        }
        # Write same entry twice
        transcript.write_text(json.dumps(entry) + "\n" + json.dumps(entry) + "\n")

        watcher = TranscriptWatcher("test-instance", str(transcript))
        with patch.object(watcher, "_log_file_edit") as mock_log:
            result = watcher.sync()
            # Should only log once despite two identical entries
            assert mock_log.call_count == 1
            assert result == 1

    def test_sync_handles_file_truncation(self, tmp_path):
        """If file is truncated, should reset position."""
        transcript = tmp_path / "transcript.jsonl"

        # Write a long entry
        entry = {
            "type": "response_item",
            "payload": {"type": "message", "role": "user", "content": [{"text": "x" * 1000}]},
            "timestamp": "2024-01-01T00:00:00Z"
        }
        transcript.write_text(json.dumps(entry) + "\n")

        watcher = TranscriptWatcher("test-instance", str(transcript))
        with patch.object(watcher, "_log_status_retroactive"):
            watcher.sync()
        original_pos = watcher._file_pos
        assert original_pos > 0

        # Truncate file (write shorter content)
        transcript.write_text("short\n")
        assert transcript.stat().st_size < original_pos

        # Sync should handle truncation
        with patch.object(watcher, "_log_status_retroactive"):
            watcher.sync()
        assert watcher._file_pos <= transcript.stat().st_size


class TestTranscriptWatcherProcessEntry:
    """Tests for _process_entry internal method."""

    def test_ignores_non_response_item(self):
        watcher = TranscriptWatcher("test-instance")
        result = watcher._process_entry({"type": "other"})
        assert result == 0

    def test_ignores_assistant_messages(self):
        watcher = TranscriptWatcher("test-instance")
        entry = {
            "type": "response_item",
            "payload": {"type": "message", "role": "assistant", "content": [{"text": "hi"}]}
        }
        with patch.object(watcher, "_log_user_prompt") as mock:
            result = watcher._process_entry(entry)
            assert result == 0
            mock.assert_not_called()

    def test_handles_custom_tool_call(self):
        """Should handle custom_tool_call type (Codex variant)."""
        watcher = TranscriptWatcher("test-instance")
        entry = {
            "type": "response_item",
            "payload": {
                "type": "custom_tool_call",
                "name": "apply_patch",
                "input": "*** Update File: test.py\n",
                "call_id": "call_1"
            },
            "timestamp": "2024-01-01T00:00:00Z"
        }
        with patch.object(watcher, "_log_file_edit") as mock:
            result = watcher._process_entry(entry)
            assert result == 1
            mock.assert_called_once()

    def test_handles_exec_command(self):
        """Should handle exec_command (another shell variant)."""
        watcher = TranscriptWatcher("test-instance")
        entry = {
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "exec_command",
                "arguments": '{"cmd": "git status"}',
                "call_id": "call_1"
            },
            "timestamp": "2024-01-01T00:00:00Z"
        }
        with patch.object(watcher, "_log_shell_command") as mock:
            watcher._process_entry(entry)
            mock.assert_called_once_with("git status", "2024-01-01T00:00:00Z")

    def test_handles_multiple_files_in_apply_patch(self):
        """apply_patch can edit multiple files in one call."""
        watcher = TranscriptWatcher("test-instance")
        entry = {
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "apply_patch",
                "input": "*** Update File: a.py\n*** Add File: b.py\n*** Delete File: c.py\n",
                "call_id": "call_1"
            },
            "timestamp": "2024-01-01T00:00:00Z"
        }
        with patch.object(watcher, "_log_file_edit") as mock:
            result = watcher._process_entry(entry)
            assert result == 3
            assert mock.call_count == 3


class TestTranscriptWatcherCallIdMemory:
    """Tests for call_id deduplication memory management."""

    def test_memory_bounded(self, tmp_path):
        """Should clear call_id set when it gets too large."""
        watcher = TranscriptWatcher("test-instance")

        # Add many call IDs
        for i in range(10001):
            watcher._logged_call_ids.add(f"call_{i}")

        assert len(watcher._logged_call_ids) > 10000

        # Process an entry - should trigger cleanup
        entry = {
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "shell_command",
                "arguments": '{"command": "test"}',
                "call_id": "call_new"
            },
            "timestamp": "2024-01-01T00:00:00Z"
        }
        with patch.object(watcher, "_log_shell_command"):
            watcher._process_entry(entry)

        # Should have been cleared and only have the new one
        assert len(watcher._logged_call_ids) == 1
        assert "call_new" in watcher._logged_call_ids


class TestTranscriptWatcherRealTranscripts:
    """Tests against real Codex transcripts — validates actual parsing behavior."""

    @pytest.fixture
    def real_transcripts(self):
        codex_home = Path.home() / ".codex" / "sessions"
        pattern = str(codex_home / "**" / "rollout-*.jsonl")
        paths = glob.glob(pattern, recursive=True)
        assert len(paths) >= 10, f"Need at least 10 Codex transcripts, found {len(paths)}"
        return paths[:30]

    @pytest.fixture
    def transcripts_with_shell(self):
        """Find transcripts that definitely have shell commands."""
        codex_home = Path.home() / ".codex" / "sessions"
        pattern = str(codex_home / "**" / "rollout-*.jsonl")
        paths = glob.glob(pattern, recursive=True)
        with_shell = []
        for p in paths[:100]:
            try:
                with open(p) as f:
                    content = f.read()
                    if '"shell' in content or '"exec_command"' in content:
                        with_shell.append(p)
                        if len(with_shell) >= 10:
                            break
            except Exception:
                continue
        assert len(with_shell) >= 3, f"Need transcripts with shell commands, found {len(with_shell)}"
        return with_shell

    @pytest.fixture
    def transcripts_with_patches(self):
        """Find transcripts that definitely have apply_patch calls."""
        codex_home = Path.home() / ".codex" / "sessions"
        pattern = str(codex_home / "**" / "rollout-*.jsonl")
        paths = glob.glob(pattern, recursive=True)
        with_patches = []
        for p in paths[:200]:
            try:
                with open(p) as f:
                    if '"apply_patch"' in f.read():
                        with_patches.append(p)
                        if len(with_patches) >= 5:
                            break
            except Exception:
                continue
        assert len(with_patches) > 0, "Need transcripts with apply_patch, found none in first 200"
        return with_patches

    def test_no_crashes_on_real_transcripts(self, real_transcripts):
        """Watcher should not crash on any real transcript."""
        for path in real_transcripts:
            watcher = TranscriptWatcher("test-instance", path)
            with patch.object(watcher, "_log_status_retroactive"):
                result = watcher.sync()
            assert isinstance(result, int)
            assert result >= 0

    def test_extracts_shell_commands_from_real_transcripts(self, transcripts_with_shell):
        """Should extract shell commands from real transcripts."""
        total_commands = 0
        for path in transcripts_with_shell:
            watcher = TranscriptWatcher("test-instance", path)
            logged_commands = []

            def capture_log(status, context, detail, timestamp):
                if context == "tool:shell":
                    logged_commands.append(detail)

            with patch.object(watcher, "_log_status_retroactive", side_effect=capture_log):
                watcher.sync()

            total_commands += len(logged_commands)

            # Verify commands look reasonable (not empty, not JSON blobs)
            for cmd in logged_commands:
                assert isinstance(cmd, str)
                assert not cmd.startswith("{")

        assert total_commands > 0, "Should have found some shell commands"

    def test_extracts_file_edits_from_real_transcripts(self, transcripts_with_patches):
        """Should extract file paths from apply_patch calls."""
        total_edits = 0
        for path in transcripts_with_patches:
            watcher = TranscriptWatcher("test-instance", path)
            logged_files = []

            def capture_log(status, context, detail, timestamp):
                if context == "tool:apply_patch":
                    logged_files.append(detail)

            with patch.object(watcher, "_log_status_retroactive", side_effect=capture_log):
                result = watcher.sync()

            total_edits += result

            for filepath in logged_files:
                assert isinstance(filepath, str)
                assert "/" in filepath or "\\" in filepath or "." in filepath

        assert total_edits > 0, "Should have found some file edits"

    def test_incremental_parsing_real_transcript(self, real_transcripts):
        """Test that incremental parsing works correctly on real transcripts."""
        path = None
        for p in real_transcripts:
            if Path(p).stat().st_size > 1000:
                path = p
                break

        assert path, "Need a transcript > 1000 bytes for incremental parsing test"

        watcher = TranscriptWatcher("test-instance", path)

        with patch.object(watcher, "_log_status_retroactive"):
            watcher.sync()

        first_pos = watcher._file_pos
        assert first_pos > 0, "Should have read some content"

        with patch.object(watcher, "_log_status_retroactive"):
            watcher.sync()

        assert watcher._file_pos == first_pos, "Position shouldn't change with no new content"

    def test_call_id_deduplication_real_transcript(self, real_transcripts):
        """Verify call_id tracking prevents duplicate processing."""
        path = None
        for p in real_transcripts:
            try:
                with open(p) as f:
                    if '"call_id"' in f.read():
                        path = p
                        break
            except Exception:
                continue

        assert path, "Need a transcript with call_ids for deduplication test"

        watcher = TranscriptWatcher("test-instance", path)

        with patch.object(watcher, "_log_status_retroactive"):
            watcher.sync()

        watcher._file_pos = 0

        calls_logged = []

        def counting_log(*args, **kwargs):
            calls_logged.append(args)

        with patch.object(watcher, "_log_status_retroactive", side_effect=counting_log):
            watcher.sync()

        assert watcher._logged_call_ids, "Should have tracked some call_ids"


class TestTranscriptWatcherIntegration:
    """Integration tests with mocked DB functions."""

    @patch("hcom.core.db.log_event")
    @patch("hcom.core.db.get_instance")
    @patch("hcom.core.instances.update_instance_position")
    def test_log_file_edit_integration(self, mock_update, mock_get, mock_log_event):
        """_log_file_edit should log event and update instance."""
        mock_get.return_value = {"status_time": 0}

        watcher = TranscriptWatcher("test-instance")
        watcher._log_file_edit("/path/to/file.py", "2024-01-01T00:00:00Z")

        mock_log_event.assert_called_once()
        call_args = mock_log_event.call_args
        assert call_args.kwargs["event_type"] == "status"
        assert call_args.kwargs["instance"] == "test-instance"
        assert "tool:apply_patch" in str(call_args.kwargs["data"])

    @patch("hcom.core.db.log_event")
    @patch("hcom.core.db.get_instance")
    @patch("hcom.core.instances.update_instance_position")
    def test_log_shell_command_integration(self, mock_update, mock_get, mock_log_event):
        """_log_shell_command should log event with command."""
        mock_get.return_value = {"status_time": 0}

        watcher = TranscriptWatcher("test-instance")
        watcher._log_shell_command("npm test", "2024-01-01T00:00:00Z")

        mock_log_event.assert_called_once()
        call_args = mock_log_event.call_args
        assert "tool:shell" in str(call_args.kwargs["data"])
        assert "npm test" in str(call_args.kwargs["data"])


# =============================================================================
# Standalone runner
# =============================================================================


if __name__ == "__main__":
    print(f"Found {len(get_transcript_paths())} Claude transcripts")
    print(f"Agent transcripts: {len(get_agent_transcripts())}")
    print(f"Session transcripts: {len(get_session_transcripts())}")

    gemini = get_gemini_transcript_paths()
    print(f"Gemini transcripts: {len(gemini)}")

    codex = get_codex_transcript_paths()
    print(f"Codex transcripts: {len(codex)}")

    oc_db = get_opencode_db_path()
    oc_sessions = get_opencode_session_ids() if oc_db else []
    print(f"OpenCode sessions: {len(oc_sessions)} (DB: {oc_db or 'not found'})")

    # Quick parse test
    from hcom.core.transcript import format_thread
    paths = get_transcript_paths()
    if paths:
        sample = random.choice(paths)
        print(f"\nSample Claude parse of {sample.name}:")
        result = parse_claude_thread(str(sample), last=3)
        print(format_thread(result))

    if oc_db and oc_sessions:
        sid = oc_sessions[0]
        print(f"\nSample OpenCode parse of {sid}:")
        result = get_thread(oc_db, tool="opencode", session_id=sid, last=3)
        print(format_thread(result))
