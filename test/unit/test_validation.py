#!/usr/bin/env python3
"""Validation and security tests for HCOM.

Tests regex patterns, message validation, command safety, and input validation.
Uses property-based testing (hypothesis) for comprehensive coverage.

Run: pytest test/test_validation.py -v
"""

import re
import time

from hypothesis import given, strategies as st, settings, assume
from hypothesis import HealthCheck
import pytest

from hcom.shared import MENTION_PATTERN
from hcom.core.tool_utils import (
    SAFE_HCOM_COMMANDS,
    build_permissions,
    build_codex_rules,
)
from hcom.commands.utils import validate_message

# ==================== Property-Based Tests ====================

class TestMentionPattern:
    """Property tests for @mention extraction"""

    @pytest.mark.slow
    @given(st.text())
    @settings(suppress_health_check=[HealthCheck.too_slow], max_examples=1000)
    def test_never_crashes(self, text):
        """Pattern should never crash on any input"""
        MENTION_PATTERN.findall(text)

    @given(st.text(alphabet='@abcdefghijklmnopqrstuvwxyz-_0123456789# \n\t'))
    def test_mentions_always_start_with_at(self, text):
        """All matches must start with @"""
        matches = MENTION_PATTERN.findall(text)
        # findall returns capture groups (without @), so check in original
        for match in matches:
            assert f'@{match}' in text

    @given(st.text())
    def test_no_email_addresses(self, text):
        """Should not match email addresses"""
        matches = MENTION_PATTERN.findall(text)
        # If we matched something, verify it's not part of an email
        for match in matches:
            mention = f'@{match}'
            idx = text.find(mention)
            if idx > 0:
                # Check char before @ is not alphanumeric/dot/dash/underscore
                prev_char = text[idx - 1]
                assert prev_char not in 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-', \
                    f"Matched email-like pattern: {text[max(0,idx-5):idx+len(match)+5]}"

    @given(st.lists(
        st.text(alphabet='abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789', min_size=1, max_size=1).flatmap(
            lambda first: st.text(alphabet='abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_:', min_size=0, max_size=19).map(
                lambda rest: first + rest
            )
        )
    ))
    def test_valid_mentions_always_match(self, usernames):
        """Valid @mentions should always be found (must start with alphanumeric, : for device suffix)"""
        for username in usernames:
            text = f"hello @{username} there"
            matches = MENTION_PATTERN.findall(text)
            assert username in matches, f"Failed to match @{username}"

    @given(st.text(alphabet='@', min_size=1, max_size=100))
    def test_multiple_at_signs_no_crash(self, text):
        """Many @ signs shouldn't cause performance issues"""
        start = time.time()
        MENTION_PATTERN.findall(text)
        elapsed = time.time() - start
        assert elapsed < 0.1, f"Too slow: {elapsed}s for {len(text)} @ signs"

    def test_mutation_resistance(self):
        """Test that mutations would break existing tests"""
        # These mutants should NOT match the same things
        mutants = [
            re.compile(r'(?<![a-zA-Z0-9._])@([\w-]+)'),   # Removed hyphen from lookbehind
            re.compile(r'(?<![a-zA-Z0-9._-])@(\w+)'),      # Removed hyphen from capture
            re.compile(r'@([\w-]+)'),                      # Removed lookbehind
            re.compile(r'(?<![a-zA-Z0-9._-])@([\w-]*)'),   # Changed + to * (allows empty)
        ]

        test_cases = [
            "user@host.com",  # Should NOT match (has alphanumeric before @)
            "@alice",         # Should match
            "test_@var",      # Should NOT match (has underscore before @)
            "@",              # Should NOT match (empty username)
            "test-@alice",    # Should NOT match (hyphen before @) - catches mutant[0]
            "@alice-bob",     # Should match 'alice-bob' - catches mutant[1]
        ]

        for i, mutant in enumerate(mutants):
            results = [mutant.findall(case) for case in test_cases]
            original_results = [MENTION_PATTERN.findall(case) for case in test_cases]
            assert results != original_results, f"Mutant {i} produces same results as original!"


class TestHcomSafeCommands:
    """Tests for centralized SAFE_HCOM_COMMANDS and derived permissions"""

    def test_safe_commands_contains_core(self):
        required = {
            'send', 'start', 'help', '--help', '-h', 'list', 'events', 'listen',
            'relay', 'config', 'transcript', 'archive', 'status', '--version', '-v',
            '--new-terminal',
        }
        assert required.issubset(set(SAFE_HCOM_COMMANDS))

    def test_safe_commands_excludes_destructive(self):
        forbidden = {'reset'}
        assert not (forbidden & set(SAFE_HCOM_COMMANDS))

    def test_claude_permissions_include_send(self):
        """Permissions include send command (detected variant only - hcom or uvx hcom)."""
        perms = build_permissions("claude")
        # Should have exactly one variant (detected at runtime)
        has_hcom = any(p.startswith("Bash(hcom send") for p in perms)
        has_uvx = any(p.startswith("Bash(uvx hcom send") for p in perms)
        assert has_hcom or has_uvx, "send command not found in permissions"
        assert not (has_hcom and has_uvx), "should only have one variant, not both"

    def test_gemini_permissions_include_send(self):
        """Permissions include send command (detected variant only - hcom or uvx hcom)."""
        perms = build_permissions("gemini")
        has_hcom = "run_shell_command(hcom send)" in perms
        has_uvx = "run_shell_command(uvx hcom send)" in perms
        assert has_hcom or has_uvx, "send command not found in permissions"
        assert not (has_hcom and has_uvx), "should only have one variant, not both"

    def test_codex_rules_include_send(self):
        """Rules include send command (detected variant only - hcom or uvx hcom)."""
        rules = build_codex_rules()
        has_hcom = 'prefix_rule(pattern=["hcom", "send"], decision="allow")' in rules
        has_uvx = 'prefix_rule(pattern=["uvx", "hcom", "send"], decision="allow")' in rules
        assert has_hcom or has_uvx, "send command not found in rules"
        assert not (has_hcom and has_uvx), "should only have one variant, not both"


# ==================== ReDoS Detection ====================

class TestReDoSVulnerabilities:
    """Test for catastrophic backtracking (exponential time complexity)"""

    def test_mention_pattern_redos(self):
        """Test MENTION_PATTERN for ReDoS"""
        evil_inputs = [
            "@" + "a" * 100 + "!",
            "a" * 100 + "@" + "b" * 100,
            "@" * 1000,
            "@" + "a_" * 100 + "!",
        ]

        for text in evil_inputs:
            start = time.time()
            MENTION_PATTERN.findall(text)
            elapsed = time.time() - start
            assert elapsed < 0.1, f"ReDoS detected in MENTION_PATTERN: {elapsed:.3f}s for {len(text)} chars"

    def test_safe_command_list_is_reasonable_size(self):
        assert 5 < len(SAFE_HCOM_COMMANDS) < 50


# ==================== Real-World Data Tests ====================

class TestRealWorldData:
    """Test patterns against real HCOM data (if available)"""

    def test_mention_extraction_from_messages(self):
        """Test MENTION_PATTERN against realistic messages"""
        messages = [
            ("Hey @alice and @bob", ['alice', 'bob']),
            ("@team-api please review", ['team-api']),
            ("email@test.com is not a mention", []),
            ("@user_123 test", ['user_123']),
            ("@@double @@ test @valid", ['double', 'valid']),  # @@ = @ followed by @double
            ("@alice @bob", ['alice', 'bob']),  # Space-separated mentions
            ("@alice!@bob", ['alice', 'bob']),  # ! is valid separator (not in lookbehind)
            ("@alice@bob", ['alice']),  # Only alice - @bob preceded by 'e' (in lookbehind)
            ("@alice:BOXE", ['alice:BOXE']),  # Remote instance with device suffix
            ("@bob:CATA test", ['bob:CATA']),  # Remote instance with word-based device ID
            ("@alice:BOXE @bob:CATA", ['alice:BOXE', 'bob:CATA']),  # Multiple remote instances
        ]

        for msg, expected in messages:
            result = MENTION_PATTERN.findall(msg)
            assert result == expected, f"Failed for: {msg!r} - got {result}, expected {expected}"

    def test_send_mentions_regex(self):
        """Ensure mention parsing still works in send-style content"""
        result = MENTION_PATTERN.findall("@alice please check this")
        assert result == ["alice"]


# ==================== Differential Testing ====================

class TestDifferentialBehavior:
    """Compare regex against simpler reference implementations"""

    def simple_mention_finder(self, text: str) -> list[str]:
        r"""Reference implementation for mention finding

        Must match regex behavior: @([a-zA-Z0-9][\w:-]*)
        - First char must be alphanumeric (letter or digit)
        - Subsequent chars can include underscore, hyphen, and colon (for device suffix)
        """
        mentions = []
        i = 0
        while i < len(text):
            if text[i] == '@':
                # Check previous char (negative lookbehind)
                if i > 0 and text[i-1] in 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789._-':
                    i += 1
                    continue

                # Extract username
                username = []
                j = i + 1

                # First character must be alphanumeric (not _ or - or :)
                if j < len(text) and text[j] in 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789':
                    username.append(text[j])
                    j += 1

                    # Rest can include underscore, hyphen, and colon
                    while j < len(text) and text[j] in 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-:':
                        username.append(text[j])
                        j += 1

                if username:
                    mentions.append(''.join(username))
                i = max(j, i + 1)  # Ensure we always advance
            else:
                i += 1

        return mentions

    @given(st.text(alphabet='@abcdefghijklmnopqrstuvwxyz0123456789-_: .\n', max_size=200))
    def test_mention_pattern_vs_simple(self, text):
        """Compare regex implementation vs simple implementation"""
        regex_result = MENTION_PATTERN.findall(text)
        simple_result = self.simple_mention_finder(text)

        assert regex_result == simple_result, \
            f"Mismatch!\nText: {text!r}\nRegex: {regex_result}\nSimple: {simple_result}"


# ==================== Boundary Analysis ====================

class TestBoundaryConditions:
    """Systematic boundary testing"""

    def test_mention_boundaries(self):
        """Test word boundary behavior for mentions"""
        cases = [
            # (text, should_match, description)
            ("@alice", True, "simple mention"),
            ("x@alice", False, "alphanumeric before"),
            ("_@alice", False, "underscore before (IS in lookbehind)"),
            (".@alice", False, "dot before"),
            ("-@alice", False, "hyphen before"),
            ("@alice ", True, "space after"),
            ("@alice.", True, "dot after"),
            ("@alice@", True, "@ after"),
            ("@@alice", True, "@ before (@ not in lookbehind)"),
            (" @alice", True, "space before"),
            ("", False, "empty string"),
            ("@", False, "@ alone"),
            ("@123", True, "digits in username"),
            ("@a-b", True, "hyphen in username"),
            ("@a_b", True, "underscore in username"),
        ]

        for text, should_match, desc in cases:
            result = MENTION_PATTERN.findall(text)
            if should_match:
                assert len(result) > 0, f"Failed: {desc} - {text!r}"
            else:
                assert len(result) == 0, f"Failed: {desc} - {text!r}"

# ==================== Command Safety Tests ====================
# NOTE: is_safe_hcom_command() removed - permissions now handled via
# native tool settings (Claude: permissions.allow, Gemini: tools.allowed)
# See hooks/settings.py and tools/gemini/settings.py


# ==================== Message Validation Tests ====================

class TestValidateMessage:
    """Test message validation logic"""

    def test_valid_messages_pass(self):
        """Normal messages should pass validation"""
        assert validate_message("Hello world") is None
        assert validate_message("Test @alice please review") is None
        assert validate_message("a" * 5000) is None  # Long but valid

    def test_empty_message_rejected(self):
        """Empty messages should be rejected"""
        error = validate_message("")
        assert error is not None
        assert "empty" in error.lower() or "message" in error.lower()

    def test_whitespace_only_rejected(self):
        """Whitespace-only messages should be rejected"""
        error = validate_message("   ")
        assert error is not None
        error = validate_message("\n\t  \n")
        assert error is not None

    def test_message_too_long_rejected(self):
        """Messages exceeding max length should be rejected"""
        # NOTE: Actual MAX_MESSAGE_SIZE is 1048576 (1MB), not 10000
        # This test documents expected behavior - adjust if 1MB is too large
        error = validate_message("a" * (1048576 + 1))
        assert error is not None
        assert "large" in error.lower() or "1048576" in error

    def test_newlines_allowed(self):
        """Messages with newlines should be allowed"""
        assert validate_message("Line 1\nLine 2\nLine 3") is None

    def test_special_chars_allowed(self):
        """Messages with special characters should be allowed"""
        assert validate_message("Hello! @user #tag $var *bold*") is None
        assert validate_message("Émojis 🎉 and unicode ñoño") is None

    @given(st.text(alphabet=st.characters(blacklist_categories=('Cc',), blacklist_characters='\x00\x01\x02\x03\x04\x05\x06\x07\x08\x0B\x0C\x0E\x0F\x10\x11\x12\x13\x14\x15\x16\x17\x18\x19\x1A\x1B\x1C\x1D\x1E\x1F'), min_size=1, max_size=5000))
    def test_reasonable_messages_never_rejected(self, msg):
        """Any reasonable-length non-empty message without control chars should pass"""
        assume(msg.strip())  # Not whitespace-only
        error = validate_message(msg)
        assert error is None, f"Rejected valid message: {msg[:100]!r}..."

    @given(st.text(min_size=0, max_size=100))
    def test_never_crashes(self, msg):
        """validate_message should never crash"""
        validate_message(msg)


if __name__ == '__main__':
    pytest.main([__file__, '-v', '--tb=short'])
