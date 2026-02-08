"""Property-based tests for hook setup/remove across Claude, Gemini, Codex.

Uses hypothesis to generate random settings files and verifies with
INDEPENDENT logic (not the same verify functions that setup uses).
"""

import json
import os
import tempfile
from pathlib import Path
from contextlib import contextmanager

import pytest
from hypothesis import given, strategies as st, settings as hyp_settings


# ==================== Strategies for generating random settings ====================

# Random hook names (non-hcom)
user_hook_names = st.text(
    alphabet="abcdefghijklmnopqrstuvwxyz-_",
    min_size=3,
    max_size=20,
).filter(lambda x: "hcom" not in x)

# Random commands (non-hcom)
user_commands = st.text(
    alphabet="abcdefghijklmnopqrstuvwxyz0123456789 -_./",
    min_size=5,
    max_size=50,
).filter(lambda x: "hcom" not in x.lower())

# User hook dict
user_hook = st.fixed_dictionaries({
    "name": user_hook_names,
    "type": st.just("command"),
    "command": user_commands,
    "timeout": st.integers(min_value=100, max_value=60000),
})

# Matcher dict with user hooks
user_matcher = st.fixed_dictionaries({
    "matcher": st.sampled_from(["*", ".*", "specific-matcher", "test.*"]),
    "hooks": st.lists(user_hook, min_size=0, max_size=3),
})

# Hook types
gemini_hook_types = ["SessionStart", "BeforeAgent", "AfterAgent", "BeforeTool",
                     "AfterTool", "Notification", "SessionEnd", "CustomType"]
claude_hook_types = ["PreToolUse", "PostToolUse", "Notification", "Stop", "CustomType"]

# Random settings structure
def random_gemini_settings():
    return st.fixed_dictionaries({
        "tools": st.fixed_dictionaries({
            "enableHooks": st.booleans(),
            "allowed": st.lists(user_commands, max_size=5),
            "autoAccept": st.booleans(),
        }),
        "model": st.fixed_dictionaries({
            "name": st.sampled_from(["gemini-2.0-flash", "gemini-2.5-pro", ""]),
            "skipNextSpeakerCheck": st.booleans(),
            "maxTurns": st.integers(min_value=1, max_value=100),
        }),
        "hooks": st.fixed_dictionaries({
            "enabled": st.booleans(),
            "disabled": st.lists(st.text(min_size=1, max_size=10), max_size=3),
            **{ht: st.lists(user_matcher, max_size=2) for ht in gemini_hook_types[:4]}
        }),
        "ui": st.fixed_dictionaries({
            "theme": st.sampled_from(["Default", "Dark", "Light"]),
        }),
    })


def random_claude_settings():
    return st.fixed_dictionaries({
        "permissions": st.fixed_dictionaries({
            "allow": st.lists(user_commands, max_size=5),
            "deny": st.lists(user_commands, max_size=3),
        }),
        "hooks": st.fixed_dictionaries({
            ht: st.lists(st.fixed_dictionaries({
                "matcher": st.text(min_size=1, max_size=20),
                "hooks": st.lists(user_hook, max_size=2),
            }), max_size=2) for ht in claude_hook_types[:3]
        }),
        "env": st.dictionaries(
            st.text(alphabet="ABCDEFGHIJKLMNOPQRSTUVWXYZ_", min_size=3, max_size=10).filter(
                lambda x: not x.startswith("HCOM")
            ),
            st.text(min_size=1, max_size=20),
            max_size=3,
        ),
    })


# ==================== Independent verification functions ====================

def independently_verify_no_hcom_hooks(settings: dict, tool: str) -> list[str]:
    """Check that NO hcom hooks exist. Returns list of violations."""
    violations = []
    hooks = settings.get("hooks", {})
    if not isinstance(hooks, dict):
        return violations

    hcom_patterns = ["hcom", "HCOM"]

    for hook_type, matchers in hooks.items():
        if hook_type in ("enabled", "disabled"):
            continue
        if not isinstance(matchers, list):
            continue
        for i, matcher in enumerate(matchers):
            if not isinstance(matcher, dict):
                continue
            for j, hook in enumerate(matcher.get("hooks", [])):
                if not isinstance(hook, dict):
                    continue
                name = hook.get("name", "")
                command = hook.get("command", "")
                if any(p in name for p in hcom_patterns) or any(p in command for p in hcom_patterns):
                    violations.append(f"{hook_type}[{i}].hooks[{j}]: name={name}, command={command}")

    return violations


def independently_verify_hcom_hooks_present(settings: dict, tool: str, expected_hooks: list) -> list[str]:
    """Check that expected hcom hooks ARE present. Returns list of missing hooks."""
    missing = []
    hooks = settings.get("hooks", {})
    if not isinstance(hooks, dict):
        return [f"hooks dict missing or invalid for {hook}" for hook in expected_hooks]

    for hook_type, expected_cmd in expected_hooks:
        found = False
        matchers = hooks.get(hook_type, [])
        if not isinstance(matchers, list):
            missing.append(f"{hook_type}: not a list")
            continue

        for matcher in matchers:
            if not isinstance(matcher, dict):
                continue
            for hook in matcher.get("hooks", []):
                if not isinstance(hook, dict):
                    continue
                cmd = hook.get("command", "")
                if expected_cmd in cmd:
                    found = True
                    break
            if found:
                break

        if not found:
            missing.append(f"{hook_type}: expected command containing '{expected_cmd}'")

    return missing


def independently_verify_user_data_preserved(original: dict, updated: dict, preserve_keys: list) -> list[str]:
    """Check that user data at specified keys is preserved. Returns violations."""
    violations = []

    def get_nested(d, keys):
        for k in keys:
            if not isinstance(d, dict) or k not in d:
                return None
            d = d[k]
        return d

    for key_path in preserve_keys:
        keys = key_path.split(".")
        orig_val = get_nested(original, keys)
        new_val = get_nested(updated, keys)
        if orig_val is not None and orig_val != new_val:
            violations.append(f"{key_path}: was {orig_val!r}, now {new_val!r}")

    return violations


# ==================== Test environment context manager ====================

@contextmanager
def isolated_gemini_env(tmp_dir: Path):
    """Context manager for isolated Gemini test environment."""
    from hcom.core.paths import clear_path_cache
    from hcom.tools.gemini import settings as gemini_settings

    test_home = tmp_dir / "home"
    test_home.mkdir(exist_ok=True)
    hcom_dir = test_home / ".hcom"
    hcom_dir.mkdir(parents=True, exist_ok=True)

    old_hcom_dir = os.environ.get("HCOM_DIR")
    old_home = os.environ.get("HOME")

    os.environ["HCOM_DIR"] = str(hcom_dir)
    os.environ["HOME"] = str(test_home)
    clear_path_cache()

    # Patch build_hcom_command
    original_build = gemini_settings.build_hcom_command
    gemini_settings.build_hcom_command = lambda: "hcom"

    try:
        yield test_home
    finally:
        gemini_settings.build_hcom_command = original_build
        if old_hcom_dir is not None:
            os.environ["HCOM_DIR"] = old_hcom_dir
        else:
            os.environ.pop("HCOM_DIR", None)
        if old_home is not None:
            os.environ["HOME"] = old_home
        clear_path_cache()


# ==================== Fixtures for non-hypothesis tests ====================

@pytest.fixture
def gemini_test_env(tmp_path, monkeypatch):
    test_home = tmp_path / "home"
    test_home.mkdir()
    hcom_dir = test_home / ".hcom"
    hcom_dir.mkdir(parents=True, exist_ok=True)
    monkeypatch.setenv("HCOM_DIR", str(hcom_dir))
    monkeypatch.setenv("HOME", str(test_home))
    from hcom.core.paths import clear_path_cache
    clear_path_cache()
    return test_home


# ==================== Gemini property tests ====================

@given(settings=random_gemini_settings())
@hyp_settings(max_examples=50, deadline=None)
def test_gemini_setup_preserves_user_data(settings):
    """Setup should preserve all non-hcom user settings."""
    from hcom.tools.gemini import settings as gemini_settings

    with tempfile.TemporaryDirectory() as tmp_dir:
        with isolated_gemini_env(Path(tmp_dir)) as test_home:
            settings_path = test_home / ".gemini" / "settings.json"
            settings_path.parent.mkdir(parents=True, exist_ok=True)

            # Extract user hooks before (filter out any that might look like hcom)
            original_user_hooks = {}
            hooks_dict = settings.get("hooks", {})
            if isinstance(hooks_dict, dict):
                for ht, matchers in hooks_dict.items():
                    if ht in ("enabled", "disabled") or not isinstance(matchers, list):
                        continue
                    user_matchers = []
                    for m in matchers:
                        if not isinstance(m, dict):
                            continue
                        user_hooks_in_matcher = [
                            h for h in m.get("hooks", [])
                            if isinstance(h, dict) and "hcom" not in h.get("name", "").lower()
                            and "hcom" not in h.get("command", "").lower()
                        ]
                        if user_hooks_in_matcher:
                            user_matchers.append({"matcher": m.get("matcher"), "hooks": user_hooks_in_matcher})
                    if user_matchers:
                        original_user_hooks[ht] = user_matchers

            settings_path.write_text(json.dumps(settings, indent=2))

            assert gemini_settings.setup_gemini_hooks() is True

            updated = json.loads(settings_path.read_text())

            # Verify user data preserved
            preserve_keys = [
                "ui.theme",
                "model.name",
                "model.maxTurns",
                "tools.autoAccept",
            ]
            violations = independently_verify_user_data_preserved(settings, updated, preserve_keys)
            assert not violations, f"User data not preserved: {violations}"

            # Verify user hooks still exist
            for ht, matchers in original_user_hooks.items():
                assert ht in updated.get("hooks", {}), f"Hook type {ht} removed"
                for orig_matcher in matchers:
                    for orig_hook in orig_matcher["hooks"]:
                        found = False
                        for m in updated["hooks"].get(ht, []):
                            for h in m.get("hooks", []):
                                if h.get("name") == orig_hook["name"]:
                                    found = True
                                    break
                        assert found, f"User hook {orig_hook['name']} in {ht} was removed"


@given(settings=random_gemini_settings())
@hyp_settings(max_examples=50, deadline=None)
def test_gemini_remove_only_removes_hcom(settings):
    """Remove should only remove hcom hooks, preserving everything else."""
    from hcom.tools.gemini import settings as gemini_settings

    with tempfile.TemporaryDirectory() as tmp_dir:
        with isolated_gemini_env(Path(tmp_dir)) as test_home:
            settings_path = test_home / ".gemini" / "settings.json"
            settings_path.parent.mkdir(parents=True, exist_ok=True)

            # First setup hooks
            settings_path.write_text(json.dumps(settings, indent=2))
            gemini_settings.setup_gemini_hooks()

            # Now remove
            assert gemini_settings.remove_gemini_hooks() is True

            updated = json.loads(settings_path.read_text())

            # Independent verification: no hcom hooks should remain
            violations = independently_verify_no_hcom_hooks(updated, "gemini")
            assert not violations, f"hcom hooks still present after remove: {violations}"


@given(settings=random_gemini_settings())
@hyp_settings(max_examples=30, deadline=None)
def test_gemini_setup_remove_roundtrip(settings):
    """Setup then remove should leave no hcom traces."""
    from hcom.tools.gemini import settings as gemini_settings

    with tempfile.TemporaryDirectory() as tmp_dir:
        with isolated_gemini_env(Path(tmp_dir)) as test_home:
            settings_path = test_home / ".gemini" / "settings.json"
            settings_path.parent.mkdir(parents=True, exist_ok=True)
            settings_path.write_text(json.dumps(settings, indent=2))

            # Setup
            assert gemini_settings.setup_gemini_hooks() is True

            # Verify hcom hooks ARE present (independent check)
            updated = json.loads(settings_path.read_text())
            expected = [(cfg[0], cfg[2]) for cfg in gemini_settings.GEMINI_HOOK_CONFIGS]
            missing = independently_verify_hcom_hooks_present(updated, "gemini", expected)
            assert not missing, f"After setup, missing hooks: {missing}"

            # Remove
            assert gemini_settings.remove_gemini_hooks() is True

            # Verify NO hcom hooks remain
            final = json.loads(settings_path.read_text())
            violations = independently_verify_no_hcom_hooks(final, "gemini")
            assert not violations, f"After remove, hcom hooks still present: {violations}"


# ==================== Edge case tests ====================

@pytest.mark.parametrize("corrupt_hooks", [
    None,  # hooks is None
    "string",  # hooks is string
    [],  # hooks is list
    {"SessionStart": "not_a_list"},  # hook type is string
    {"SessionStart": [None, "string", 123]},  # matchers are wrong types
    {"SessionStart": [{"matcher": "*", "hooks": "not_a_list"}]},  # hooks in matcher is string
])
def test_gemini_handles_malformed_hooks(corrupt_hooks, gemini_test_env, monkeypatch):
    """Setup/remove should handle malformed hooks gracefully."""
    from hcom.tools.gemini import settings as gemini_settings

    test_home = gemini_test_env
    monkeypatch.setattr(gemini_settings, "build_hcom_command", lambda: "hcom")

    settings_path = test_home / ".gemini" / "settings.json"
    settings_path.parent.mkdir(parents=True, exist_ok=True)

    settings = {"hooks": corrupt_hooks, "ui": {"theme": "Dark"}}
    settings_path.write_text(json.dumps(settings, indent=2))

    # Should not crash
    result = gemini_settings.setup_gemini_hooks()
    # May succeed or fail depending on corruption, but should not raise

    # User data should still be there
    updated = json.loads(settings_path.read_text())
    assert updated.get("ui", {}).get("theme") == "Dark"


def test_gemini_handles_empty_file(gemini_test_env, monkeypatch):
    """Setup should work on empty settings file."""
    from hcom.tools.gemini import settings as gemini_settings

    test_home = gemini_test_env
    monkeypatch.setattr(gemini_settings, "build_hcom_command", lambda: "hcom")

    settings_path = test_home / ".gemini" / "settings.json"
    settings_path.parent.mkdir(parents=True, exist_ok=True)
    settings_path.write_text("{}")

    assert gemini_settings.setup_gemini_hooks() is True

    updated = json.loads(settings_path.read_text())
    expected = [(cfg[0], cfg[2]) for cfg in gemini_settings.GEMINI_HOOK_CONFIGS]
    missing = independently_verify_hcom_hooks_present(updated, "gemini", expected)
    assert not missing, f"Missing hooks: {missing}"


def test_gemini_handles_no_file(gemini_test_env, monkeypatch):
    """Setup should work when settings file doesn't exist."""
    from hcom.tools.gemini import settings as gemini_settings

    test_home = gemini_test_env
    monkeypatch.setattr(gemini_settings, "build_hcom_command", lambda: "hcom")

    settings_path = test_home / ".gemini" / "settings.json"
    assert not settings_path.exists()

    assert gemini_settings.setup_gemini_hooks() is True
    assert settings_path.exists()

    updated = json.loads(settings_path.read_text())
    expected = [(cfg[0], cfg[2]) for cfg in gemini_settings.GEMINI_HOOK_CONFIGS]
    missing = independently_verify_hcom_hooks_present(updated, "gemini", expected)
    assert not missing, f"Missing hooks: {missing}"


def test_gemini_idempotent_setup(gemini_test_env, monkeypatch):
    """Running setup twice should produce same result."""
    from hcom.tools.gemini import settings as gemini_settings

    test_home = gemini_test_env
    monkeypatch.setattr(gemini_settings, "build_hcom_command", lambda: "hcom")

    settings_path = test_home / ".gemini" / "settings.json"
    settings_path.parent.mkdir(parents=True, exist_ok=True)
    settings_path.write_text(json.dumps({"ui": {"theme": "Dark"}}))

    assert gemini_settings.setup_gemini_hooks() is True
    first = json.loads(settings_path.read_text())

    assert gemini_settings.setup_gemini_hooks() is True
    second = json.loads(settings_path.read_text())

    # Should be identical (no duplicate hooks)
    assert first == second


def test_gemini_mixed_hcom_and_user_hooks(gemini_test_env, monkeypatch):
    """User hooks in same matcher as hcom hooks should be preserved."""
    from hcom.tools.gemini import settings as gemini_settings

    test_home = gemini_test_env
    monkeypatch.setattr(gemini_settings, "build_hcom_command", lambda: "hcom")

    settings_path = test_home / ".gemini" / "settings.json"
    settings_path.parent.mkdir(parents=True, exist_ok=True)

    # Setup with mixed hooks in same matcher
    settings = {
        "hooks": {
            "enabled": True,
            "SessionStart": [{
                "matcher": "*",
                "hooks": [
                    {"name": "hcom-sessionstart", "type": "command",
                     "command": "hcom gemini-sessionstart", "timeout": 5000},
                    {"name": "my-logger", "type": "command",
                     "command": "echo session started", "timeout": 1000},
                ]
            }]
        }
    }
    settings_path.write_text(json.dumps(settings, indent=2))

    assert gemini_settings.remove_gemini_hooks() is True

    updated = json.loads(settings_path.read_text())

    # User hook should remain
    session_hooks = updated.get("hooks", {}).get("SessionStart", [])
    assert len(session_hooks) == 1
    hooks_list = session_hooks[0].get("hooks", [])
    assert len(hooks_list) == 1
    assert hooks_list[0]["name"] == "my-logger"

    # No hcom hooks
    violations = independently_verify_no_hcom_hooks(updated, "gemini")
    assert not violations


# ==================== Codex test environment ====================

@contextmanager
def isolated_codex_env(tmp_dir: Path):
    """Context manager for isolated Codex test environment."""
    from hcom.core.paths import clear_path_cache
    from hcom.tools.codex import settings as codex_settings

    test_home = tmp_dir / "home"
    test_home.mkdir(exist_ok=True)
    hcom_dir = test_home / ".hcom"
    hcom_dir.mkdir(parents=True, exist_ok=True)

    old_hcom_dir = os.environ.get("HCOM_DIR")
    old_home = os.environ.get("HOME")

    os.environ["HCOM_DIR"] = str(hcom_dir)
    os.environ["HOME"] = str(test_home)
    clear_path_cache()

    original_build = codex_settings.build_hcom_command
    codex_settings.build_hcom_command = lambda: "hcom"

    try:
        yield test_home
    finally:
        codex_settings.build_hcom_command = original_build
        if old_hcom_dir is not None:
            os.environ["HCOM_DIR"] = old_hcom_dir
        else:
            os.environ.pop("HCOM_DIR", None)
        if old_home is not None:
            os.environ["HOME"] = old_home
        clear_path_cache()


# ==================== Codex strategies ====================

def random_toml_value():
    """Generate random TOML-safe values."""
    return st.one_of(
        st.text(alphabet="abcdefghijklmnopqrstuvwxyz0123456789-_.", min_size=1, max_size=20),
        st.integers(min_value=0, max_value=1000),
        st.booleans(),
    )


def random_codex_config():
    """Generate random Codex TOML config content."""
    return st.builds(
        lambda sections, extra_lines: _build_toml_content(sections, extra_lines),
        sections=st.lists(
            st.tuples(
                st.text(alphabet="abcdefghijklmnopqrstuvwxyz", min_size=3, max_size=10),  # section name
                st.dictionaries(
                    st.text(alphabet="abcdefghijklmnopqrstuvwxyz_", min_size=2, max_size=10),
                    random_toml_value(),
                    max_size=3,
                ),
            ),
            max_size=3,
        ),
        extra_lines=st.lists(
            st.text(alphabet="abcdefghijklmnopqrstuvwxyz =_\"0123456789", min_size=5, max_size=30)
            .filter(lambda x: "notify" not in x.lower() and "hcom" not in x.lower()),
            max_size=3,
        ),
    )


def _build_toml_content(sections: list, extra_lines: list) -> str:
    """Build TOML content from sections and extra lines."""
    lines = []
    # Add extra lines at top (before sections)
    for line in extra_lines:
        if "=" in line and not line.strip().startswith("#"):
            lines.append(line)
    lines.append("")
    # Add sections
    for section_name, section_dict in sections:
        lines.append(f"[{section_name}]")
        for key, value in section_dict.items():
            if isinstance(value, str):
                lines.append(f'{key} = "{value}"')
            elif isinstance(value, bool):
                lines.append(f'{key} = {"true" if value else "false"}')
            else:
                lines.append(f'{key} = {value}')
        lines.append("")
    return "\n".join(lines)


# ==================== Codex independent verification ====================

def independently_verify_no_hcom_in_toml(content: str) -> list[str]:
    """Check that NO hcom hook references exist in TOML content.

    Only flags actual hook config lines, not:
    - Section headers like [hcom]
    - Comments
    - Random user config that happens to contain "hcom" as part of another word

    Detection: notify line must contain "hcom" (the command).
    "codex-notify" alone is too generic - could be someone's unrelated script.
    """
    violations = []
    for i, line in enumerate(content.splitlines(), 1):
        stripped = line.strip()

        # Skip comments and section headers
        if stripped.startswith("#") or stripped.startswith("["):
            continue

        # Check for hcom notify hook specifically
        # Must be a notify line that contains "hcom" command
        if stripped.startswith("notify") and "=" in stripped:
            # Only flag if "hcom" appears (the actual command)
            # This catches: ["hcom", "codex-notify"] and ["uvx", "hcom", "codex-notify"]
            if "hcom" in stripped.lower():
                violations.append(f"Line {i}: {stripped}")

    return violations


def independently_verify_hcom_notify_present(content: str) -> bool:
    """Check that hcom notify hook is present."""
    for line in content.splitlines():
        if line.strip().startswith("notify") and "codex-notify" in line:
            return True
    return False


# ==================== Codex property tests ====================

@given(config_content=random_codex_config())
@hyp_settings(max_examples=50, deadline=None)
def test_codex_setup_preserves_user_config(config_content):
    """Setup should preserve all non-hcom user config."""
    from hcom.tools.codex import settings as codex_settings

    with tempfile.TemporaryDirectory() as tmp_dir:
        with isolated_codex_env(Path(tmp_dir)) as test_home:
            config_path = test_home / ".codex" / "config.toml"
            config_path.parent.mkdir(parents=True, exist_ok=True)

            # Extract sections before setup
            original_sections = []
            for line in config_content.splitlines():
                if line.strip().startswith("["):
                    original_sections.append(line.strip())

            config_path.write_text(config_content)

            result = codex_settings.setup_codex_hooks(include_permissions=False)
            # May fail if config is too malformed, that's ok
            if not result:
                return

            updated = config_path.read_text()

            # All original sections should still exist
            for section in original_sections:
                assert section in updated, f"Section {section} was removed"


@given(config_content=random_codex_config())
@hyp_settings(max_examples=50, deadline=None)
def test_codex_remove_only_removes_hcom(config_content):
    """Remove should only remove hcom hook, preserving everything else."""
    from hcom.tools.codex import settings as codex_settings

    with tempfile.TemporaryDirectory() as tmp_dir:
        with isolated_codex_env(Path(tmp_dir)) as test_home:
            config_path = test_home / ".codex" / "config.toml"
            config_path.parent.mkdir(parents=True, exist_ok=True)

            config_path.write_text(config_content)
            codex_settings.setup_codex_hooks(include_permissions=False)

            assert codex_settings.remove_codex_hooks() is True

            updated = config_path.read_text()

            violations = independently_verify_no_hcom_in_toml(updated)
            assert not violations, f"hcom still present after remove: {violations}"


@given(config_content=random_codex_config())
@hyp_settings(max_examples=30, deadline=None)
def test_codex_setup_remove_roundtrip(config_content):
    """Setup then remove should leave no hcom traces."""
    from hcom.tools.codex import settings as codex_settings

    with tempfile.TemporaryDirectory() as tmp_dir:
        with isolated_codex_env(Path(tmp_dir)) as test_home:
            config_path = test_home / ".codex" / "config.toml"
            config_path.parent.mkdir(parents=True, exist_ok=True)
            config_path.write_text(config_content)

            result = codex_settings.setup_codex_hooks(include_permissions=False)
            if not result:
                return  # Setup failed (e.g., existing non-hcom notify)

            updated = config_path.read_text()
            assert independently_verify_hcom_notify_present(updated), "hcom notify not present after setup"

            assert codex_settings.remove_codex_hooks() is True

            final = config_path.read_text()
            violations = independently_verify_no_hcom_in_toml(final)
            assert not violations, f"hcom still present after remove: {violations}"


# ==================== Codex edge case tests ====================

@pytest.mark.parametrize("corrupt_content", [
    "",  # Empty file
    "   \n\n   ",  # Whitespace only
    "# Just a comment",  # Comment only
    "invalid toml [ stuff",  # Malformed
    "[section]\nkey = value\n[another",  # Incomplete section
])
def test_codex_handles_malformed_config(corrupt_content, tmp_path, monkeypatch):
    """Setup/remove should handle malformed config gracefully."""
    from hcom.tools.codex import settings as codex_settings
    from hcom.core.paths import clear_path_cache

    test_home = tmp_path / "home"
    test_home.mkdir()
    hcom_dir = test_home / ".hcom"
    hcom_dir.mkdir(parents=True, exist_ok=True)
    monkeypatch.setenv("HCOM_DIR", str(hcom_dir))
    monkeypatch.setenv("HOME", str(test_home))
    clear_path_cache()
    monkeypatch.setattr(codex_settings, "build_hcom_command", lambda: "hcom")

    config_path = test_home / ".codex" / "config.toml"
    config_path.parent.mkdir(parents=True, exist_ok=True)
    config_path.write_text(corrupt_content)

    # Should not crash
    codex_settings.setup_codex_hooks(include_permissions=False)
    codex_settings.remove_codex_hooks()


def test_codex_idempotent_setup(tmp_path, monkeypatch):
    """Running setup twice should produce same result."""
    from hcom.tools.codex import settings as codex_settings
    from hcom.core.paths import clear_path_cache

    test_home = tmp_path / "home"
    test_home.mkdir()
    hcom_dir = test_home / ".hcom"
    hcom_dir.mkdir(parents=True, exist_ok=True)
    monkeypatch.setenv("HCOM_DIR", str(hcom_dir))
    monkeypatch.setenv("HOME", str(test_home))
    clear_path_cache()
    monkeypatch.setattr(codex_settings, "build_hcom_command", lambda: "hcom")

    config_path = test_home / ".codex" / "config.toml"
    config_path.parent.mkdir(parents=True, exist_ok=True)
    config_path.write_text("[model]\nname = \"test\"\n")

    assert codex_settings.setup_codex_hooks(include_permissions=False) is True
    first = config_path.read_text()

    assert codex_settings.setup_codex_hooks(include_permissions=False) is True
    second = config_path.read_text()

    assert first == second, "Setup not idempotent"
    # Count actual notify config lines (not the substring in "codex-notify")
    notify_line_count = sum(1 for line in second.splitlines()
                           if line.strip().startswith("notify") and "=" in line)
    assert notify_line_count == 1, f"Expected 1 notify line, got {notify_line_count}"


# ==================== Claude test environment ====================

@contextmanager
def isolated_claude_env(tmp_dir: Path):
    """Context manager for isolated Claude test environment."""
    from hcom.core.paths import clear_path_cache
    from hcom.tools.claude import settings as claude_settings

    test_home = tmp_dir / "home"
    test_home.mkdir(exist_ok=True)
    hcom_dir = test_home / ".hcom"
    hcom_dir.mkdir(parents=True, exist_ok=True)

    old_hcom_dir = os.environ.get("HCOM_DIR")
    old_home = os.environ.get("HOME")

    os.environ["HCOM_DIR"] = str(hcom_dir)
    os.environ["HOME"] = str(test_home)
    clear_path_cache()

    # Patch build_hcom_command and _get_hook_command
    original_build = claude_settings.build_hcom_command
    original_hook_cmd = claude_settings._get_hook_command
    claude_settings.build_hcom_command = lambda: "hcom"
    claude_settings._get_hook_command = lambda: "hcom"

    try:
        yield test_home
    finally:
        claude_settings.build_hcom_command = original_build
        claude_settings._get_hook_command = original_hook_cmd
        if old_hcom_dir is not None:
            os.environ["HCOM_DIR"] = old_hcom_dir
        else:
            os.environ.pop("HCOM_DIR", None)
        if old_home is not None:
            os.environ["HOME"] = old_home
        clear_path_cache()


# ==================== Claude property tests ====================

@given(settings=random_claude_settings())
@hyp_settings(max_examples=50, deadline=None)
def test_claude_setup_preserves_user_data(settings):
    """Setup should preserve all non-hcom user settings."""
    from hcom.tools.claude import settings as claude_settings

    with tempfile.TemporaryDirectory() as tmp_dir:
        with isolated_claude_env(Path(tmp_dir)) as test_home:
            settings_path = test_home / ".claude" / "settings.json"
            settings_path.parent.mkdir(parents=True, exist_ok=True)

            # Extract user hooks before (filter out any that might look like hcom)
            original_user_hooks = {}
            hooks_dict = settings.get("hooks", {})
            if isinstance(hooks_dict, dict):
                for ht, matchers in hooks_dict.items():
                    if not isinstance(matchers, list):
                        continue
                    user_matchers = []
                    for m in matchers:
                        if not isinstance(m, dict):
                            continue
                        user_hooks_in_matcher = [
                            h for h in m.get("hooks", [])
                            if isinstance(h, dict) and "hcom" not in h.get("name", "").lower()
                            and "hcom" not in h.get("command", "").lower()
                        ]
                        if user_hooks_in_matcher:
                            user_matchers.append({"matcher": m.get("matcher"), "hooks": user_hooks_in_matcher})
                    if user_matchers:
                        original_user_hooks[ht] = user_matchers

            settings_path.write_text(json.dumps(settings, indent=2))

            assert claude_settings.setup_claude_hooks(include_permissions=False) is True

            updated = json.loads(settings_path.read_text())

            # Verify user data preserved (env and permissions.deny only - allow gets hcom entries added)
            # Note: env gets HCOM added, so we check specific user keys, not the whole env
            preserve_keys = ["permissions.deny"]
            violations = independently_verify_user_data_preserved(settings, updated, preserve_keys)
            assert not violations, f"User data not preserved: {violations}"

            # User env keys should still exist (HCOM is added by setup)
            for key in settings.get("env", {}):
                assert key in updated.get("env", {}), f"User env key {key} was removed"

            # Verify user hooks still exist
            for ht, matchers in original_user_hooks.items():
                assert ht in updated.get("hooks", {}), f"Hook type {ht} removed"
                for orig_matcher in matchers:
                    for orig_hook in orig_matcher["hooks"]:
                        found = False
                        for m in updated["hooks"].get(ht, []):
                            for h in m.get("hooks", []):
                                if h.get("name") == orig_hook["name"]:
                                    found = True
                                    break
                        assert found, f"User hook {orig_hook['name']} in {ht} was removed"


@given(settings=random_claude_settings())
@hyp_settings(max_examples=50, deadline=None)
def test_claude_remove_only_removes_hcom(settings):
    """Remove should only remove hcom hooks, preserving everything else."""
    from hcom.tools.claude import settings as claude_settings

    with tempfile.TemporaryDirectory() as tmp_dir:
        with isolated_claude_env(Path(tmp_dir)) as test_home:
            settings_path = test_home / ".claude" / "settings.json"
            settings_path.parent.mkdir(parents=True, exist_ok=True)

            # First setup hooks
            settings_path.write_text(json.dumps(settings, indent=2))
            claude_settings.setup_claude_hooks(include_permissions=False)

            # Now remove
            assert claude_settings.remove_claude_hooks() is True

            updated = json.loads(settings_path.read_text())

            # Independent verification: no hcom hooks should remain
            violations = independently_verify_no_hcom_hooks(updated, "claude")
            assert not violations, f"hcom hooks still present after remove: {violations}"


@given(settings=random_claude_settings())
@hyp_settings(max_examples=30, deadline=None)
def test_claude_setup_remove_roundtrip(settings):
    """Setup then remove should leave no hcom traces."""
    from hcom.tools.claude import settings as claude_settings

    with tempfile.TemporaryDirectory() as tmp_dir:
        with isolated_claude_env(Path(tmp_dir)) as test_home:
            settings_path = test_home / ".claude" / "settings.json"
            settings_path.parent.mkdir(parents=True, exist_ok=True)
            settings_path.write_text(json.dumps(settings, indent=2))

            # Setup
            assert claude_settings.setup_claude_hooks(include_permissions=False) is True

            # Verify hcom hooks ARE present (independent check)
            updated = json.loads(settings_path.read_text())
            # Check a few key hook types are present
            expected = [("PostToolUse", "post"), ("Stop", "poll"), ("Notification", "notify")]
            missing = independently_verify_hcom_hooks_present(updated, "claude", expected)
            assert not missing, f"After setup, missing hooks: {missing}"

            # Remove
            assert claude_settings.remove_claude_hooks() is True

            # Verify NO hcom hooks remain
            final = json.loads(settings_path.read_text())
            violations = independently_verify_no_hcom_hooks(final, "claude")
            assert not violations, f"After remove, hcom hooks still present: {violations}"


# ==================== Claude edge case tests ====================

@pytest.fixture
def claude_test_env(tmp_path, monkeypatch):
    test_home = tmp_path / "home"
    test_home.mkdir()
    hcom_dir = test_home / ".hcom"
    hcom_dir.mkdir(parents=True, exist_ok=True)
    monkeypatch.setenv("HCOM_DIR", str(hcom_dir))
    monkeypatch.setenv("HOME", str(test_home))
    from hcom.core.paths import clear_path_cache
    clear_path_cache()
    return test_home


@pytest.mark.parametrize("corrupt_hooks", [
    None,  # hooks is None
    "string",  # hooks is string
    [],  # hooks is list
    {"PreToolUse": "not_a_list"},  # hook type is string
    {"PreToolUse": [None, "string", 123]},  # matchers are wrong types
    {"PreToolUse": [{"matcher": "*", "hooks": "not_a_list"}]},  # hooks in matcher is string
])
def test_claude_handles_malformed_hooks(corrupt_hooks, claude_test_env, monkeypatch):
    """Setup/remove should handle malformed hooks gracefully."""
    from hcom.tools.claude import settings as claude_settings

    test_home = claude_test_env
    monkeypatch.setattr(claude_settings, "build_hcom_command", lambda: "hcom")
    monkeypatch.setattr(claude_settings, "_get_hook_command", lambda: "hcom")

    settings_path = test_home / ".claude" / "settings.json"
    settings_path.parent.mkdir(parents=True, exist_ok=True)

    settings = {"hooks": corrupt_hooks, "env": {"MY_VAR": "test"}}
    settings_path.write_text(json.dumps(settings, indent=2))

    # Should not crash
    result = claude_settings.setup_claude_hooks(include_permissions=False)
    # May succeed or fail depending on corruption, but should not raise

    # User data should still be there
    updated = json.loads(settings_path.read_text())
    assert updated.get("env", {}).get("MY_VAR") == "test"


def test_claude_handles_empty_file(claude_test_env, monkeypatch):
    """Setup should work on empty settings file."""
    from hcom.tools.claude import settings as claude_settings

    test_home = claude_test_env
    monkeypatch.setattr(claude_settings, "build_hcom_command", lambda: "hcom")
    monkeypatch.setattr(claude_settings, "_get_hook_command", lambda: "hcom")

    settings_path = test_home / ".claude" / "settings.json"
    settings_path.parent.mkdir(parents=True, exist_ok=True)
    settings_path.write_text("{}")

    assert claude_settings.setup_claude_hooks(include_permissions=False) is True

    updated = json.loads(settings_path.read_text())
    # Check some hooks are present
    assert "hooks" in updated
    assert "PostToolUse" in updated["hooks"]


def test_claude_handles_no_file(claude_test_env, monkeypatch):
    """Setup should work when settings file doesn't exist."""
    from hcom.tools.claude import settings as claude_settings

    test_home = claude_test_env
    monkeypatch.setattr(claude_settings, "build_hcom_command", lambda: "hcom")
    monkeypatch.setattr(claude_settings, "_get_hook_command", lambda: "hcom")

    settings_path = test_home / ".claude" / "settings.json"
    assert not settings_path.exists()

    assert claude_settings.setup_claude_hooks(include_permissions=False) is True
    assert settings_path.exists()

    updated = json.loads(settings_path.read_text())
    assert "hooks" in updated


def test_claude_idempotent_setup(claude_test_env, monkeypatch):
    """Running setup twice should produce same result."""
    from hcom.tools.claude import settings as claude_settings

    test_home = claude_test_env
    monkeypatch.setattr(claude_settings, "build_hcom_command", lambda: "hcom")
    monkeypatch.setattr(claude_settings, "_get_hook_command", lambda: "hcom")

    settings_path = test_home / ".claude" / "settings.json"
    settings_path.parent.mkdir(parents=True, exist_ok=True)
    settings_path.write_text(json.dumps({"env": {"MY_VAR": "test"}}))

    assert claude_settings.setup_claude_hooks(include_permissions=False) is True
    first = json.loads(settings_path.read_text())

    assert claude_settings.setup_claude_hooks(include_permissions=False) is True
    second = json.loads(settings_path.read_text())

    # Should be identical (no duplicate hooks)
    assert first == second


def test_claude_mixed_hcom_and_user_hooks(claude_test_env, monkeypatch):
    """User hooks in same type as hcom hooks should be preserved."""
    from hcom.tools.claude import settings as claude_settings

    test_home = claude_test_env
    monkeypatch.setattr(claude_settings, "build_hcom_command", lambda: "hcom")
    monkeypatch.setattr(claude_settings, "_get_hook_command", lambda: "hcom")

    settings_path = test_home / ".claude" / "settings.json"
    settings_path.parent.mkdir(parents=True, exist_ok=True)

    # Setup with mixed hooks in same type
    settings = {
        "hooks": {
            "PostToolUse": [{
                "matcher": "",
                "hooks": [
                    {"type": "command", "command": "hcom post"},
                    {"type": "command", "command": "echo user hook", "name": "my-logger"},
                ]
            }]
        }
    }
    settings_path.write_text(json.dumps(settings, indent=2))

    assert claude_settings.remove_claude_hooks() is True

    updated = json.loads(settings_path.read_text())

    # User hook should remain
    post_hooks = updated.get("hooks", {}).get("PostToolUse", [])
    assert len(post_hooks) == 1
    hooks_list = post_hooks[0].get("hooks", [])
    assert len(hooks_list) == 1
    assert hooks_list[0].get("command") == "echo user hook"

    # No hcom hooks
    violations = independently_verify_no_hcom_hooks(updated, "claude")
    assert not violations
