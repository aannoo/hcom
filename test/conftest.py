"""
Pytest configuration and shared fixtures for hcom tests
"""
import os
import sys
import time
import pytest
from pathlib import Path
from typing import Mapping

log_file_path = Path(__file__).parent / 'output' / 'unit' / 'unit_test_latest.log'
# Ensure log directory exists (fresh checkout / CI sandbox)
log_file_path.parent.mkdir(parents=True, exist_ok=True)

# Ensure tests import the local worktree package (not another editable install).
# Walk up to find repo root (works whether this is test/conftest.py or test/public/conftest.py)
_repo_root = Path(__file__).resolve().parent
while _repo_root != _repo_root.parent and not (_repo_root / "pyproject.toml").exists():
    _repo_root = _repo_root.parent
_src_dir = _repo_root / "src"
if _src_dir.exists():
    sys.path.insert(0, str(_src_dir))
    # Also set HCOM_DEV_ROOT so subprocess calls to hcom CLI route to local code
    os.environ["HCOM_DEV_ROOT"] = str(_repo_root)

# Default test timeout (short for tests)
DEFAULT_TEST_TIMEOUT = 5

# Test configuration values (written to config.toml)
BASE_TEST_CONFIG = {
    "terminal": {"active": "print"},  # Don't actually launch terminals in tests
    "launch": {
        "tag": "", "hints": "", "notes": "",
        "subagent_timeout": 30, "auto_subscribe": "collision",
        "claude": {"args": "Say hi in chat"},
        "gemini": {"args": "", "system_prompt": ""},
        "codex": {"args": "", "sandbox_mode": "workspace", "system_prompt": ""},
    },
    "relay": {"url": "", "id": "", "token": "", "enabled": True},
    "preferences": {"timeout": DEFAULT_TEST_TIMEOUT, "auto_approve": True, "name_export": ""},
}

# Environment variables that affect hcom behavior — must be cleared for test isolation
IDENTITY_ENV_VARS = [
    'CLAUDECODE',           # Inside Claude Code detection
    'HCOM_NAME',            # Instance identity
    'HCOM_LAUNCHED',        # hcom launched flag
    'HCOM_LAUNCH_EVENT_ID', # Event ID at launch (affects last_event_id)
    'HCOM_PTY_MODE',        # PTY mode (skips poll loop in Stop hook)
    'GEMINI_CLI',           # Inside Gemini CLI
    'HCOM_PROCESS_ID',      # Process identity binding (primary identity mechanism)
]

# Config env vars that override config.toml file values
CONFIG_ENV_VARS = [
    'HCOM_TIMEOUT',
    'HCOM_SUBAGENT_TIMEOUT',
    'HCOM_TERMINAL',
    'HCOM_HINTS',
    'HCOM_CLAUDE_ARGS',
    'HCOM_GEMINI_ARGS',
    'HCOM_CODEX_ARGS',
    'HCOM_TAG',
    'HCOM_AGENT',
]


def clean_test_env(base_env: Mapping[str, str] | None = None) -> dict[str, str]:
    """Create clean test environment with identity vars explicitly cleared.
    Args:
        base_env: Base environment to start from (default: os.environ)
    Returns:
        Dictionary with all identity vars removed for consistent test behavior
    """
    env = dict(base_env or os.environ)

    # Clear all identity-related env vars
    for var in IDENTITY_ENV_VARS:
        env.pop(var, None)

    # Clear config env vars so config.toml file values are used instead
    for var in CONFIG_ENV_VARS:
        env.pop(var, None)

    # Isolate from production relay — tests must never push to real server
    env.pop("HCOM_RELAY", None)
    env.pop("HCOM_RELAY_TOKEN", None)
    env.pop("HCOM_RELAY_ENABLED", None)

    return env

# Use pytest_runtest_makereport hook to capture test results
@pytest.hookimpl(hookwrapper=True)
def pytest_runtest_makereport(item, call):
    """Capture test results and log them"""
    outcome = yield
    report = outcome.get_result()
    
    if report.when == 'call':  # Only log actual test execution, not setup/teardown
        with open(log_file_path, 'a', encoding='utf-8') as f:
            if report.passed:
                f.write(f"PASSED: {report.nodeid}\n")
            elif report.failed:
                f.write(f"FAILED: {report.nodeid}\n")
                if report.longrepr:
                    # Limit error message length to avoid huge logs
                    error_str = str(report.longrepr)[:200]
                    f.write(f"  Error: {error_str}\n")
            elif report.skipped:
                f.write(f"SKIPPED: {report.nodeid}\n")


# ==================== Shared Fixtures ====================

@pytest.fixture(autouse=True)
def clear_hcom_caches():
    """Automatically clear hcom path cache before each test.
    
    This ensures tests that mock HOME/HCOM_DIR environment variables
    don't use cached paths from previous tests or the test runner.
    """
    from hcom.core.paths import clear_path_cache
    clear_path_cache()
    yield
    # Clear again after test to prevent pollution
    clear_path_cache()


@pytest.fixture
def temp_home(tmp_path, monkeypatch):
    """Provides a temporary HOME directory for tests"""
    monkeypatch.setenv('HOME', str(tmp_path))
    # CRITICAL: Set HCOM_DIR for Windows (Path.home() ignores HOME on Windows)
    monkeypatch.setenv('HCOM_DIR', str(tmp_path / '.hcom'))
    monkeypatch.setattr('hcom.core.config._config_cache', None)  # Clear config cache
    # Clear HCOM env vars to prevent leakage from running HCOM session
    monkeypatch.delenv('HCOM_NAME', raising=False)
    monkeypatch.delenv('HCOM_TERMINAL', raising=False)
    monkeypatch.delenv('HCOM_TIMEOUT', raising=False)
    monkeypatch.delenv('HCOM_SUBAGENT_TIMEOUT', raising=False)
    monkeypatch.delenv('HCOM_CLAUDE_ARGS', raising=False)
    monkeypatch.delenv('HCOM_HINTS', raising=False)
    monkeypatch.delenv('HCOM_TAG', raising=False)
    monkeypatch.delenv('HCOM_AGENT', raising=False)

    # Close any existing database connection to prevent cross-test pollution
    from hcom.core.db import close_db
    close_db()

    return tmp_path


@pytest.fixture
def hcom_env(temp_home, monkeypatch):
    """Unified test environment with HOME isolation and config setup"""
    from hcom.core.paths import ensure_hcom_directories

    # Clear identity env vars that might leak from the test runner
    for var in IDENTITY_ENV_VARS:
        monkeypatch.delenv(var, raising=False)

    # Isolate from production relay
    monkeypatch.delenv("HCOM_RELAY", raising=False)
    monkeypatch.delenv("HCOM_RELAY_TOKEN", raising=False)
    monkeypatch.delenv("HCOM_RELAY_ENABLED", raising=False)

    hcom_dir = temp_home / '.hcom'
    hcom_dir.mkdir(parents=True, exist_ok=True)

    # Write config.toml
    import tomli_w
    config_file = hcom_dir / 'config.toml'
    config_file.write_text(tomli_w.dumps(BASE_TEST_CONFIG), encoding='utf-8')

    # Force reload config to use the test config file
    monkeypatch.setattr('hcom.core.config._config_cache', None)

    # Replicate production environment: ensure all directories exist
    ensure_hcom_directories()

    # Create instances directory
    instances_dir = hcom_dir / 'instances'
    instances_dir.mkdir(exist_ok=True)

    return {
        "home": temp_home,
        "hcom_dir": hcom_dir,
        "instances_dir": instances_dir,
        "db_file": hcom_dir / 'hcom.db',
    }


@pytest.fixture
def isolated_hcom_env(hcom_env):
    """Backward compatibility fixture - returns hcom_dir path"""
    return hcom_env["hcom_dir"]


@pytest.fixture
def make_instance(hcom_env):
    """Helper to create instance in DB"""
    from hcom.core.db import init_db, set_session_binding, save_instance

    def _make(name, data=None):
        init_db()

        # Build full data dict with defaults (row exists = participating)
        defaults = {
            "name": name,
            "last_event_id": 0,
            "status": "active",
            "created_at": time.time()
        }
        if data:
            defaults.update(data)

        # Save to DB
        save_instance(name, defaults)

        # Create session binding if session_id provided (required for message participation)
        session_id = defaults.get('session_id')
        if session_id:
            set_session_binding(session_id, name)

        return name
    return _make
