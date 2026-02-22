"""OpenCode plugin management for hcom.

Manages the hcom.ts plugin file in OpenCode's config directory:
- Install: Copy hcom.ts to ~/.config/opencode/plugins/hcom.ts
- Verify: Check if plugin is installed in any scanned directory
- Remove: Delete hcom.ts from all plugin directories (both plugin/ and plugins/)
- Ensure: Idempotent install for launcher auto-setup on first launch

Plugin source: tools/opencode/data/hcom.ts (bundled in package)
Install target: $XDG_CONFIG_HOME/opencode/plugins/hcom.ts (default: ~/.config/opencode/plugins/)

OpenCode scans both plugin/ and plugins/ subdirectories under its config dir.
Uninstall checks both to ensure clean removal.
"""

from __future__ import annotations

import logging
import os
import shutil
from pathlib import Path

log = logging.getLogger(__name__)

PLUGIN_FILENAME = "hcom.ts"


def _get_global_plugin_dir() -> Path:
    """Return the canonical plugin install directory.

    Uses XDG_CONFIG_HOME with fallback to ~/.config.
    Returns: ~/.config/opencode/plugins/ (or XDG equivalent)
    """
    config_home = os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config"))
    return Path(config_home) / "opencode" / "plugins"


def _get_plugin_source() -> Path:
    """Return path to the bundled hcom.ts plugin source (data/hcom.ts next to this file)."""
    return Path(__file__).resolve().parent / "data" / PLUGIN_FILENAME


def _scan_plugin_dirs() -> list[Path]:
    """Return all directories where hcom.ts plugin might exist.

    Checks both plugin/ and plugins/ under:
    - ~/.config/opencode/ (or XDG equivalent)
    - OPENCODE_CONFIG_DIR if set

    Returns only directories that actually exist on disk.
    """
    candidates: list[Path] = []

    # Standard XDG config location
    config_home = os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config"))
    opencode_base = Path(config_home) / "opencode"
    candidates.append(opencode_base / "plugin")
    candidates.append(opencode_base / "plugins")

    # OPENCODE_CONFIG_DIR override
    custom_dir = os.environ.get("OPENCODE_CONFIG_DIR")
    if custom_dir:
        custom_base = Path(custom_dir)
        candidates.append(custom_base / "plugin")
        candidates.append(custom_base / "plugins")

    return [d for d in candidates if d.exists()]


def get_opencode_plugin_path() -> Path:
    """Return the canonical install path for the hcom.ts plugin."""
    return _get_global_plugin_dir() / PLUGIN_FILENAME


def verify_opencode_plugin_installed(*, check_permissions: bool = False) -> bool:
    """Check if hcom.ts plugin is installed in any OpenCode plugin directory.

    Args:
        check_permissions: Unused for OpenCode (API compatibility with other tool
            verify functions). OpenCode permissions are set at launch via env var,
            not baked into the plugin file.

    Returns True if hcom.ts exists in any scanned plugin directory or the
    canonical install directory.
    """
    # Check canonical path
    if get_opencode_plugin_path().exists():
        return True

    # Check all scanned directories
    for d in _scan_plugin_dirs():
        if (d / PLUGIN_FILENAME).exists():
            return True

    return False


def install_opencode_plugin(*, include_permissions: bool = False) -> bool:
    """Copy hcom.ts plugin from source to the canonical plugin directory.

    Creates ~/.config/opencode/plugins/ if it doesn't exist.
    Uses shutil.copy2 for metadata preservation.

    Args:
        include_permissions: Unused for OpenCode (API compatibility with other tool
            setup functions). OpenCode permissions are set at launch via env var.

    Returns True on success. Raises on failure (FileNotFoundError for missing
    source, OSError for filesystem issues).
    """
    source = _get_plugin_source()
    if not source.exists():
        raise FileNotFoundError(f"Plugin source not found: {source}")

    target_dir = _get_global_plugin_dir()
    target = target_dir / PLUGIN_FILENAME

    target_dir.mkdir(parents=True, exist_ok=True)
    # Remove stale symlinks (e.g., from previous dev installs) before copying
    if target.is_symlink() or target.exists():
        target.unlink()
    shutil.copy2(source, target)
    return True


def remove_opencode_plugin() -> None:
    """Remove hcom.ts from ALL OpenCode plugin directories.

    Scans both plugin/ and plugins/ subdirectories plus the canonical install
    directory. Silently skips if file doesn't exist.

    Raises OSError on permission/IO failures.
    """
    # Collect all paths to check
    paths_to_check: list[Path] = [get_opencode_plugin_path()]
    for d in _scan_plugin_dirs():
        p = d / PLUGIN_FILENAME
        if p not in paths_to_check:
            paths_to_check.append(p)

    for p in paths_to_check:
        if p.exists():
            p.unlink()


def _plugin_is_current() -> bool:
    """Check if the installed plugin matches the bundled source content."""
    source = _get_plugin_source()
    installed = get_opencode_plugin_path()
    if not source.exists() or not installed.exists():
        return False
    return source.read_bytes() == installed.read_bytes()


def ensure_plugin_installed() -> bool:
    """Ensure the hcom.ts plugin is installed and up to date.

    Used by the launcher for auto-install on first launch.
    Compares file contents so plugin updates propagate automatically.
    """
    if verify_opencode_plugin_installed() and _plugin_is_current():
        return True
    return install_opencode_plugin()


__all__ = [
    "get_opencode_plugin_path",
    "verify_opencode_plugin_installed",
    "install_opencode_plugin",
    "remove_opencode_plugin",
    "ensure_plugin_installed",
    "PLUGIN_FILENAME",
]
