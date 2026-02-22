"""Native binary discovery - single source of truth.

Binary lookup order:
1. Bundled binary in package data (pip install with platform wheel)
2. PATH lookup (maturin install or manual symlink)

If no binary found, Python fallback is used (__main__.py falls through to cli.py).
"""

import os
import platform
import shutil
import threading
from pathlib import Path

_cached_binary: str | None = None
_cache_checked: bool = False
_binary_lock = threading.Lock()


def _get_platform_tag() -> str:
    """Get platform tag for binary lookup.

    Returns: e.g., 'darwin-arm64', 'darwin-x86_64', 'linux-x86_64', 'windows-x86_64'
    """
    system = platform.system().lower()
    machine = platform.machine().lower()

    # Normalize machine names
    if machine in ("x86_64", "amd64"):
        machine = "x86_64"
    elif machine in ("arm64", "aarch64"):
        machine = "arm64"

    return f"{system}-{machine}"


def _get_bundled_binary() -> str | None:
    """Look for bundled binary in package data.

    On noexec filesystems (e.g. Android sdcard), copies the binary to
    ~/.hcom/bin/ where it can be executed.
    """
    try:
        # Get the bin directory relative to this module
        bin_dir = Path(__file__).parent.parent / "bin"
        if not bin_dir.is_dir():
            return None

        plat_tag = _get_platform_tag()

        # Look for platform-specific binary
        # Naming: hcom-darwin-arm64, hcom-linux-x86_64, etc.
        binary_name = f"hcom-{plat_tag}"
        if platform.system().lower() == "windows":
            binary_name += ".exe"

        binary_path = bin_dir / binary_name
        if not binary_path.is_file():
            return None

        if os.access(binary_path, os.X_OK):
            return str(binary_path)

        # Binary exists but isn't executable (noexec filesystem) — copy to ~/.hcom/bin/
        exec_dir = Path.home() / ".hcom" / "bin"
        exec_dir.mkdir(parents=True, exist_ok=True)
        exec_path = exec_dir / binary_name
        src_mtime = binary_path.stat().st_mtime
        if not exec_path.is_file() or exec_path.stat().st_mtime < src_mtime:
            shutil.copy2(binary_path, exec_path)
            exec_path.chmod(0o755)
        if os.access(exec_path, os.X_OK):
            return str(exec_path)

        return None
    except Exception:
        return None


def get_native_binary() -> str | None:
    """Find hcom binary path.

    Priority:
    1. Bundled binary in package data (platform wheel)
    2. PATH lookup (maturin install or manual symlink)

    Result is cached for process lifetime.
    """
    global _cached_binary, _cache_checked

    if _cache_checked:
        return _cached_binary

    with _binary_lock:
        if _cache_checked:
            return _cached_binary

        # 1. Bundled binary in package
        if bundled := _get_bundled_binary():
            _cached_binary = bundled
            _cache_checked = True
            return _cached_binary

        # 2. PATH lookup (manual install)
        # Verify it's a real binary, not a Python script (avoids recursive exec
        # when the fallback wheel's entry point script is the only 'hcom' on PATH)
        found = shutil.which("hcom")
        if found:
            try:
                with open(found, "rb") as f:
                    if not f.read(2).startswith(b"#!"):
                        _cached_binary = found
            except OSError:
                pass
        _cache_checked = True
        return _cached_binary


def is_dev_mode() -> bool:
    """Check if running in dev mode (via hdev script)."""
    return bool(os.environ.get("HCOM_DEV_ROOT"))


def is_native_available() -> bool:
    """Check if native binary is available."""
    return get_native_binary() is not None


def get_platform_tag() -> str:
    """Get current platform tag (public API for CI scripts)."""
    return _get_platform_tag()
