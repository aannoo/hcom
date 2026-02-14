"""File system utilities and path management"""

from __future__ import annotations
import os
import time
import json
import tempfile
import threading
from pathlib import Path
from typing import Callable, Any, TextIO

from .thread_context import get_hcom_dir_str, get_cwd
from ..shared import IS_WINDOWS, HcomError

# Constants
FILE_RETRY_DELAY = 0.01  # 10ms delay for file lock retries

# Path constants
LOGS_DIR = ".tmp/logs"
LAUNCH_DIR = ".tmp/launch"
FLAGS_DIR = ".tmp/flags"
LAUNCHES_DIR = "launches"  # Saved launch profiles
CONFIG_FILE = "config.env"  # Legacy — used only for migration
CONFIG_TOML = "config.toml"
ENV_FILE = "env"
ARCHIVE_DIR = "archive"

# Cache for resolved hcom directory (per-process, thread-safe)
_hcom_dir_cache: Path | None = None
_hcom_dir_cache_key: tuple[str | None, str | None] | None = None  # (HCOM_DIR, CWD if relative)
_hcom_dir_cache_lock = threading.Lock()

# ==================== Path Utilities ====================


def _get_hcom_dir_cache_key() -> tuple[str | None, str | None]:
    """Cache key for resolved hcom dir.

    If HCOM_DIR is relative, its resolved meaning depends on CWD, so include CWD.
    Uses thread-safe accessors for daemon compatibility.
    """
    hcom_dir = get_hcom_dir_str()
    if not hcom_dir:
        return (None, None)
    try:
        p = Path(hcom_dir).expanduser()
    except Exception:
        # Weird env var values shouldn't crash caching; just make it non-cacheable per-cwd.
        return (hcom_dir, str(get_cwd()))

    if p.is_absolute():
        return (hcom_dir, None)
    return (hcom_dir, str(get_cwd()))


def _resolve_hcom_dir() -> Path:
    """Resolve hcom directory. Global-only with explicit HCOM_DIR override.

    Resolution order:
    1. HCOM_DIR env var → use it (relative or absolute)
    2. ~/.hcom writable → use it
    3. Error: clear message with sandbox instructions

    No automatic fallback to ./.hcom. One env var for sandboxed environments.
    Uses thread-safe accessors for daemon compatibility.
    """
    # 1. Explicit override (relative or absolute)
    if hcom_dir := get_hcom_dir_str():
        # Expand ~ and resolve relative paths against CWD
        return Path(hcom_dir).expanduser().resolve()  # Returns absolute

    # 2. Global default, no fallback
    try:
        global_path = Path.home() / ".hcom"
        global_path.mkdir(parents=True, exist_ok=True)
        return global_path
    except (PermissionError, OSError, RuntimeError) as e:
        raise HcomError(
            f"Cannot write to ~/.hcom: {e}\n\n"
            "For sandboxed/containerized environments:\n"
            '  export HCOM_DIR="$PWD/.hcom"\n\n'
            "Then retry your command."
        )


def hcom_path(*parts: str, ensure_parent: bool = False) -> Path:
    """Build path under hcom directory.

    Resolution order:
    1. HCOM_DIR env var (explicit override, relative or absolute)
    2. ~/.hcom (global default)
    3. Error with sandbox instructions

    Result is cached per-process for performance.
    """
    global _hcom_dir_cache, _hcom_dir_cache_key

    current_key = _get_hcom_dir_cache_key()

    # Thread-safe cache check and update
    with _hcom_dir_cache_lock:
        if _hcom_dir_cache is None or _hcom_dir_cache_key != current_key:
            _hcom_dir_cache = _resolve_hcom_dir()
            _hcom_dir_cache_key = current_key

        path = _hcom_dir_cache

    if parts:
        path = path.joinpath(*parts)
    if ensure_parent:
        path.parent.mkdir(parents=True, exist_ok=True)
    return path


def get_project_root() -> Path:
    """Get the project root for hook installation.

    Returns parent of hcom_path():
    - ~/.hcom → home directory (~/)
    - HCOM_DIR=/workspace/.hcom → /workspace/

    Convention: HCOM_DIR should end in '.hcom' (e.g., /workspace/.hcom).
    If user sets HCOM_DIR=/tmp/myhcom, hooks go to /tmp/.claude/ - this is
    intentional and consistent with "project root = parent of hcom data dir".

    Used for anchoring tool config files (.claude/, .gemini/, .codex/).
    """
    return hcom_path().parent


def is_hcom_dir_override() -> bool:
    """Check if HCOM_DIR env var is set. Uses thread-safe accessor."""
    return bool(get_hcom_dir_str())


def clear_path_cache() -> None:
    """Clear the cached hcom directory path. Call after changing HCOM_DIR or CWD."""
    global _hcom_dir_cache, _hcom_dir_cache_key
    with _hcom_dir_cache_lock:
        _hcom_dir_cache = None
        _hcom_dir_cache_key = None


def ensure_hcom_dir() -> Path:
    """Ensure hcom directory exists and is writable.

    Uses hcom_path() which uses HCOM_DIR or ~/.hcom.

    Raises:
        HcomError: If directory cannot be created or written to.
    """
    path = hcom_path()  # Already resolved
    try:
        path.mkdir(parents=True, exist_ok=True)
        return path
    except (PermissionError, OSError) as e:
        raise HcomError(f"Cannot write to {path}: {e}")


def ensure_hcom_directories() -> bool:
    """Ensure all critical HCOM directories exist. Idempotent, safe to call repeatedly.
    Called at hook entry to support opt-in scenarios where hooks execute before CLI commands.
    Returns True on success, False on failure."""
    try:
        for dir_name in [LOGS_DIR, LAUNCH_DIR, FLAGS_DIR, LAUNCHES_DIR, ARCHIVE_DIR]:
            hcom_path(dir_name).mkdir(parents=True, exist_ok=True)
        return True
    except (OSError, PermissionError):
        return False


def launches_dir() -> Path:
    """Get path to launches directory (~/.hcom/launches/), creating if needed."""
    path = hcom_path(LAUNCHES_DIR)
    path.mkdir(parents=True, exist_ok=True)
    return path


# ==================== Atomic File Operations ====================


def atomic_write(filepath: str | Path, content: str) -> bool:
    """Write content to file atomically to prevent corruption. Includes Windows file-locking retry logic. Returns True on success, False on failure."""
    filepath = Path(filepath) if not isinstance(filepath, Path) else filepath
    filepath.parent.mkdir(parents=True, exist_ok=True)

    # Create temp file once (outside retry loop to prevent leaks)
    with tempfile.NamedTemporaryFile(
        mode="w", encoding="utf-8", delete=False, dir=filepath.parent, suffix=".tmp"
    ) as tmp:
        tmp.write(content)
        tmp.flush()
        os.fsync(tmp.fileno())
        tmp_name = tmp.name

    # Retry only the replace operation
    for attempt in range(3):
        try:
            os.replace(tmp_name, filepath)
            return True
        except PermissionError:
            if IS_WINDOWS and attempt < 2:
                time.sleep(FILE_RETRY_DELAY)
                continue
            else:
                try:  # Clean up temp file on final failure
                    Path(tmp_name).unlink()
                except (FileNotFoundError, PermissionError, OSError):
                    pass
                return False
        except Exception:
            try:  # Clean up temp file on any other error
                os.unlink(tmp_name)
            except (FileNotFoundError, PermissionError, OSError):
                pass
            return False

    return False  # All attempts exhausted


def increment_flag_counter(name: str) -> int:
    """Increment a counter in .tmp/flags/{name} and return new value."""
    flag_file = hcom_path(FLAGS_DIR, name)
    flag_file.parent.mkdir(parents=True, exist_ok=True)

    count = 0
    if flag_file.exists():
        try:
            count = int(flag_file.read_text().strip())
        except (ValueError, OSError):
            count = 0

    count += 1
    atomic_write(flag_file, str(count))
    return count


def get_flag_counter(name: str) -> int:
    """Get current value of a counter in .tmp/flags/{name}."""
    flag_file = hcom_path(FLAGS_DIR, name)
    if not flag_file.exists():
        return 0
    try:
        return int(flag_file.read_text().strip())
    except (ValueError, OSError):
        return 0


def read_file_with_retry(
    filepath: str | Path,
    read_func: Callable[[TextIO], Any],
    default: Any = None,
    max_retries: int = 3,
) -> Any:
    """Read file with retry logic for Windows file locking"""
    if not Path(filepath).exists():
        return default

    for attempt in range(max_retries):
        try:
            with open(filepath, "r", encoding="utf-8") as f:
                return read_func(f)
        except PermissionError:
            # Only retry on Windows (file locking issue)
            if IS_WINDOWS and attempt < max_retries - 1:
                time.sleep(FILE_RETRY_DELAY)
            else:
                # Re-raise on Unix or after max retries on Windows
                if not IS_WINDOWS:
                    raise  # Unix permission errors are real issues
                break  # Windows: return default after retries
        except (json.JSONDecodeError, FileNotFoundError, IOError):
            break  # Don't retry on other errors

    return default


__all__ = [
    "hcom_path",
    "ensure_hcom_dir",
    "ensure_hcom_directories",
    "launches_dir",
    "atomic_write",
    "read_file_with_retry",
    "increment_flag_counter",
    "get_flag_counter",
    # HCOM_DIR support
    "is_hcom_dir_override",
    "get_project_root",
    "clear_path_cache",
    # Path constants
    "LOGS_DIR",
    "LAUNCH_DIR",
    "FLAGS_DIR",
    "LAUNCHES_DIR",
    "CONFIG_FILE",
    "CONFIG_TOML",
    "ENV_FILE",
    "ARCHIVE_DIR",
]
