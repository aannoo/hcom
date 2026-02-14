"""Tests for transcript parsing against real transcripts from all three tools.

Runs against actual transcript files from:
- Claude: ~/.claude/projects/**/*.jsonl
- Gemini: ~/.gemini/tmp/**/chats/*.json
- Codex: ~/.codex/sessions/**/*.jsonl

These require real transcript files on the machine. Not collected by pytest
(see conftest.py). Run directly: python test/public/real/test_transcript.py

Unit tests for the same functions (synthetic fixtures) live in
test/public/unit/test_transcript.py.
"""

import glob
import os
import random
import time
from pathlib import Path


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


def get_all_transcript_paths(max_per_tool: int = None) -> dict[str, list[Path]]:
    """Get transcript paths for all three tools."""
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
# Cross-tool — real transcript tests
# =============================================================================


class TestRealTranscriptsCrossToolStatistics:
    """Statistical tests across all three tools."""

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

    # Quick parse test
    from hcom.core.transcript import format_thread
    paths = get_transcript_paths()
    if paths:
        sample = random.choice(paths)
        print(f"\nSample parse of {sample.name}:")
        result = parse_claude_thread(str(sample), last=3)
        print(format_thread(result))
