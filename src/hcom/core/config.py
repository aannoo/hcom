"""Configuration management - central config system used by all modules."""

from __future__ import annotations

import os
import sys
import re
import shlex
import threading
import tomllib
from dataclasses import dataclass, fields
from pathlib import Path
from typing import Any

from .paths import hcom_path, atomic_write, CONFIG_FILE, CONFIG_TOML, ENV_FILE
from ..shared import parse_env_value, format_env_value


# ==================== TOML Schema ====================

TOML_HEADER = """\
# hcom configuration
# Help: hcom config --help
# Docs: hcom run docs
"""

# Canonical TOML structure with built-in defaults
DEFAULT_TOML: dict[str, Any] = {
    "terminal": {"active": "default"},
    "relay": {"url": "", "id": "", "token": "", "enabled": True},
    "launch": {
        "tag": "",
        "hints": "",
        "notes": "",
        "subagent_timeout": 30,
        "auto_subscribe": "collision",
        "claude": {"args": ""},
        "gemini": {"args": "", "system_prompt": ""},
        "codex": {"args": "", "sandbox_mode": "workspace", "system_prompt": ""},
    },
    "preferences": {"timeout": 86400, "auto_approve": True, "name_export": ""},
}

# Bidirectional mapping: HcomConfig field name <-> TOML dotted path
TOML_KEY_MAP: dict[str, str] = {
    "terminal": "terminal.active",
    "tag": "launch.tag",
    "hints": "launch.hints",
    "notes": "launch.notes",
    "subagent_timeout": "launch.subagent_timeout",
    "auto_subscribe": "launch.auto_subscribe",
    "claude_args": "launch.claude.args",
    "gemini_args": "launch.gemini.args",
    "gemini_system_prompt": "launch.gemini.system_prompt",
    "codex_args": "launch.codex.args",
    "codex_sandbox_mode": "launch.codex.sandbox_mode",
    "codex_system_prompt": "launch.codex.system_prompt",
    "relay": "relay.url",
    "relay_id": "relay.id",
    "relay_token": "relay.token",
    "relay_enabled": "relay.enabled",
    "timeout": "preferences.timeout",
    "auto_approve": "preferences.auto_approve",
    "name_export": "preferences.name_export",
}

# Mapping: HcomConfig field name -> HCOM_* env var key
_FIELD_TO_ENV: dict[str, str] = {
    "timeout": "HCOM_TIMEOUT",
    "subagent_timeout": "HCOM_SUBAGENT_TIMEOUT",
    "terminal": "HCOM_TERMINAL",
    "hints": "HCOM_HINTS",
    "notes": "HCOM_NOTES",
    "tag": "HCOM_TAG",
    "claude_args": "HCOM_CLAUDE_ARGS",
    "gemini_args": "HCOM_GEMINI_ARGS",
    "codex_args": "HCOM_CODEX_ARGS",
    "codex_sandbox_mode": "HCOM_CODEX_SANDBOX_MODE",
    "gemini_system_prompt": "HCOM_GEMINI_SYSTEM_PROMPT",
    "codex_system_prompt": "HCOM_CODEX_SYSTEM_PROMPT",
    "relay": "HCOM_RELAY",
    "relay_id": "HCOM_RELAY_ID",
    "relay_token": "HCOM_RELAY_TOKEN",
    "relay_enabled": "HCOM_RELAY_ENABLED",
    "auto_approve": "HCOM_AUTO_APPROVE",
    "auto_subscribe": "HCOM_AUTO_SUBSCRIBE",
    "name_export": "HCOM_NAME_EXPORT",
}
_ENV_TO_FIELD = {v: k for k, v in _FIELD_TO_ENV.items()}

# Relay fields — file-only, no env var override
_RELAY_FIELDS = {"relay", "relay_id", "relay_token", "relay_enabled"}

# Derive KNOWN_CONFIG_KEYS and DEFAULT_KNOWN_VALUES for backward compat
# (config_cmd.py display loop iterates KNOWN_CONFIG_KEYS)
KNOWN_CONFIG_KEYS: list[str] = list(_FIELD_TO_ENV.values())
DEFAULT_KNOWN_VALUES: dict[str, str] = {}
_default_config_obj = None  # Lazy — avoid constructing HcomConfig at import time


def _get_default_known_values() -> dict[str, str]:
    """Lazily build DEFAULT_KNOWN_VALUES from HcomConfig defaults."""
    global DEFAULT_KNOWN_VALUES, _default_config_obj
    if not DEFAULT_KNOWN_VALUES:
        # Build from dataclass defaults directly
        _defaults = {f.name: f.default for f in fields(HcomConfig)}
        for field_name, env_key in _FIELD_TO_ENV.items():
            val = _defaults.get(field_name, "")
            if isinstance(val, bool):
                DEFAULT_KNOWN_VALUES[env_key] = "1" if val else "0"
            else:
                DEFAULT_KNOWN_VALUES[env_key] = str(val)
    return DEFAULT_KNOWN_VALUES


# ==================== TOML I/O ====================

_TERMINAL_DANGEROUS_CHARS = ["`", "$", ";", "|", "&", "\n", "\r"]


def _get_nested(data: dict, dotted_path: str) -> Any:
    """Get value from nested dict using dotted path. Returns None if missing."""
    parts = dotted_path.split(".")
    current = data
    for part in parts:
        if not isinstance(current, dict) or part not in current:
            return None
        current = current[part]
    return current


def _set_nested(data: dict, dotted_path: str, value: Any) -> None:
    """Set value in nested dict using dotted path, creating intermediates."""
    parts = dotted_path.split(".")
    current = data
    for part in parts[:-1]:
        if part not in current or not isinstance(current[part], dict):
            current[part] = {}
        current = current[part]
    current[parts[-1]] = value


def load_toml_config(path: Path) -> dict[str, Any]:
    """Read config.toml and return flat dict of HcomConfig field names -> values.

    Includes terminal dangerous-char validation (rejects unsafe terminal values).
    """
    try:
        raw = tomllib.loads(path.read_text(encoding="utf-8"))
    except tomllib.TOMLDecodeError as exc:
        print(f"Warning: Failed to parse {path.name}: {exc} — using defaults", file=sys.stderr)
        return {}
    except (FileNotFoundError, PermissionError, UnicodeDecodeError):
        return {}

    result: dict[str, Any] = {}
    for field_name, toml_path in TOML_KEY_MAP.items():
        val = _get_nested(raw, toml_path)
        if val is not None:
            result[field_name] = val

    # Terminal dangerous-char validation
    terminal_val = result.get("terminal")
    if isinstance(terminal_val, str) and any(c in terminal_val for c in _TERMINAL_DANGEROUS_CHARS):
        print(
            f"Warning: Unsafe characters in terminal.active "
            f"({', '.join(repr(c) for c in _TERMINAL_DANGEROUS_CHARS if c in terminal_val)}), "
            f"ignoring custom terminal command",
            file=sys.stderr,
        )
        del result["terminal"]

    return result


def _config_to_toml_dict(config: HcomConfig) -> dict[str, Any]:
    """Convert HcomConfig to nested TOML-ready dict."""
    import copy
    toml_data = copy.deepcopy(DEFAULT_TOML)
    for field_name, toml_path in TOML_KEY_MAP.items():
        value = getattr(config, field_name)
        _set_nested(toml_data, toml_path, value)
    return toml_data


def save_toml_config(config: HcomConfig, presets: dict[str, dict] | None = None) -> None:
    """Write config.toml from HcomConfig. Optionally includes terminal presets."""
    toml_data = _config_to_toml_dict(config)

    # Merge terminal presets if provided
    if presets:
        toml_data.setdefault("terminal", {})["presets"] = presets

    import tomli_w
    content = TOML_HEADER + "\n" + tomli_w.dumps(toml_data)
    toml_path = hcom_path(CONFIG_TOML, ensure_parent=True)
    atomic_write(toml_path, content)

    # Invalidate settings cache (presets may have changed)
    from .settings import invalidate_settings_cache
    invalidate_settings_cache()


def _load_toml_presets(path: Path) -> dict[str, dict]:
    """Load terminal presets from config.toml [terminal.presets.*] section."""
    try:
        raw = tomllib.loads(path.read_text(encoding="utf-8"))
    except (tomllib.TOMLDecodeError, OSError, ValueError):
        return {}
    terminal = raw.get("terminal", {})
    if not isinstance(terminal, dict):
        return {}
    presets = terminal.get("presets", {})
    return presets if isinstance(presets, dict) else {}


# ==================== Env File I/O ====================

ENV_HEADER = "# Env vars passed through to agents (e.g. ANTHROPIC_MODEL=...)\n"
DEFAULT_ENV_VARS = ["ANTHROPIC_MODEL", "CLAUDE_CODE_SUBAGENT_MODEL", "GEMINI_MODEL"]


def load_env_extras(path: Path) -> dict[str, str]:
    """Load non-HCOM env vars from env file."""
    if not path.exists():
        return {}
    result: dict[str, str] = {}
    try:
        for line in path.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            if "=" in line:
                key, _, value = line.partition("=")
                key = key.strip()
                # Ignore any HCOM_* keys that ended up in env file
                if key and not key.startswith("HCOM_"):
                    result[key] = parse_env_value(value)
    except (FileNotFoundError, PermissionError, UnicodeDecodeError):
        pass
    return result


def _save_env_file(extras: dict[str, str]) -> None:
    """Write env passthrough file (non-HCOM vars only)."""
    env_path = hcom_path(ENV_FILE, ensure_parent=True)
    lines = [ENV_HEADER]
    # Always include default env var placeholders
    all_keys = dict.fromkeys(DEFAULT_ENV_VARS)
    all_keys.update(extras)
    for key in all_keys:
        if key.startswith("HCOM_"):
            continue
        value = extras.get(key, "")
        formatted = format_env_value(value)
        lines.append(f"{key}={formatted}" if formatted else f"{key}=")
    content = "\n".join(lines) + "\n"
    atomic_write(env_path, content)


# ==================== Migration ====================


def _migrate_config_env_to_toml() -> bool:
    """Migrate config.env + settings.toml → config.toml + env.

    Returns True if migration succeeded (or was unnecessary), False on failure.
    Atomic: old files deleted only after both new files written successfully.
    """
    config_env_path = hcom_path(CONFIG_FILE)
    if not config_env_path.exists():
        return True  # Nothing to migrate

    toml_path = hcom_path(CONFIG_TOML)
    if toml_path.exists():
        return True  # Already migrated

    try:
        # 1. Parse config.env — raw key=value only, NO validation.
        # parse_env_file() validates terminal against presets from config.toml,
        # but config.toml doesn't exist yet during migration.
        file_config: dict[str, str] = {}
        for line in config_env_path.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            if "=" in line:
                key, _, value = line.partition("=")
                key = key.strip()
                if key:
                    file_config[key] = parse_env_value(value.strip())

        # 2. Split HCOM_* vs non-HCOM
        hcom_values: dict[str, str] = {}
        env_extras: dict[str, str] = {}
        for key, value in file_config.items():
            if key.startswith("HCOM_"):
                hcom_values[key] = value
            else:
                env_extras[key] = value

        # 3. Normalize compat values
        if hcom_values.get("HCOM_CODEX_SANDBOX_MODE") == "full-auto":
            hcom_values["HCOM_CODEX_SANDBOX_MODE"] = "danger-full-access"
        # Clear old default kickstart prompt
        claude_args = hcom_values.get("HCOM_CLAUDE_ARGS", "")
        if claude_args.strip().strip("'\"") == "say hi in hcom chat":
            hcom_values["HCOM_CLAUDE_ARGS"] = ""

        # 4. Build TOML data from HCOM_* values
        import copy
        toml_data = copy.deepcopy(DEFAULT_TOML)
        for env_key, value in hcom_values.items():
            field_name = _ENV_TO_FIELD.get(env_key)
            if field_name and field_name in TOML_KEY_MAP:
                toml_path_str = TOML_KEY_MAP[field_name]
                # Convert types for TOML
                default_val = _get_nested(DEFAULT_TOML, toml_path_str)
                typed_val: bool | int | str
                if isinstance(default_val, bool):
                    typed_val = value not in ("0", "false", "False", "no", "off", "")
                elif isinstance(default_val, int):
                    try:
                        typed_val = int(value)
                    except (ValueError, TypeError):
                        typed_val = value  # Let validation catch it
                else:
                    typed_val = value
                _set_nested(toml_data, toml_path_str, typed_val)

        # 5. Merge terminal presets from settings.toml
        settings_path = hcom_path("settings.toml")
        if settings_path.exists():
            try:
                settings_data = tomllib.loads(settings_path.read_text(encoding="utf-8"))
                terminal_presets = settings_data.get("terminal", {})
                if isinstance(terminal_presets, dict) and terminal_presets:
                    toml_data.setdefault("terminal", {})["presets"] = terminal_presets
            except Exception:
                pass

        # 6. Atomic write config.toml
        import tomli_w
        toml_content = TOML_HEADER + "\n" + tomli_w.dumps(toml_data)
        toml_dest = hcom_path(CONFIG_TOML, ensure_parent=True)
        if not atomic_write(toml_dest, toml_content):
            return False

        # 7. Atomic write env file (non-HCOM vars only, skip empty)
        env_lines = [ENV_HEADER]
        all_env_keys = dict.fromkeys(DEFAULT_ENV_VARS)
        all_env_keys.update(env_extras)
        for key in all_env_keys:
            if key.startswith("HCOM_"):
                continue
            value = env_extras.get(key, "")
            formatted = format_env_value(value)
            env_lines.append(f"{key}={formatted}" if formatted else f"{key}=")
        env_content = "\n".join(env_lines) + "\n"
        env_dest = hcom_path(ENV_FILE, ensure_parent=True)
        if not atomic_write(env_dest, env_content):
            # Rollback: remove config.toml since env write failed
            try:
                toml_dest.unlink()
            except Exception:
                pass
            return False

        # 8. Delete old files only after both writes succeed
        try:
            config_env_path.unlink()
        except Exception:
            pass
        try:
            if settings_path.exists():
                settings_path.unlink()
        except Exception:
            pass

        return True

    except Exception as exc:
        from .log import log_warn
        log_warn("config", "migration_failed", f"Failed to migrate config.env to config.toml: {exc}")
        return False


# ==================== Config Error ====================


class HcomConfigError(ValueError):
    """Raised when HcomConfig contains invalid values."""

    def __init__(self, errors: dict[str, str]):
        self.errors = errors
        if errors:
            message = "Invalid config:\n" + "\n".join(f"  - {msg}" for msg in errors.values())
        else:
            message = "Invalid config"
        super().__init__(message)


# ==================== Config Dataclass ====================


@dataclass
class HcomConfig:
    """HCOM configuration with validation. Load priority: env → config.toml → defaults"""

    timeout: int = 86400  # Idle timeout - 24hr since last activity (CC hook max)
    subagent_timeout: int = 30
    terminal: str = "default"
    hints: str = ""
    notes: str = ""
    tag: str = ""
    claude_args: str = ""
    gemini_args: str = ""
    codex_args: str = ""
    codex_sandbox_mode: str = "workspace"
    gemini_system_prompt: str = ""
    codex_system_prompt: str = ""
    relay: str = ""
    relay_id: str = ""
    relay_token: str = ""
    relay_enabled: bool = True
    auto_approve: bool = True
    auto_subscribe: str = "collision"
    name_export: str = ""

    def __post_init__(self):
        """Validate configuration on construction"""
        errors = self.collect_errors()
        if errors:
            raise HcomConfigError(errors)

    def validate(self) -> list[str]:
        """Validate all fields, return list of errors"""
        return list(self.collect_errors().values())

    def collect_errors(self) -> dict[str, str]:
        """Validate fields and return dict of field → error message"""
        errors: dict[str, str] = {}

        def set_error(field: str, message: str) -> None:
            if field in errors:
                errors[field] = f"{errors[field]}; {message}"
            else:
                errors[field] = message

        # Validate timeout
        if isinstance(self.timeout, bool):
            set_error(
                "timeout",
                f"timeout must be an integer, not boolean (got {self.timeout})",
            )
        elif not isinstance(self.timeout, int):
            set_error(
                "timeout",
                f"timeout must be an integer, got {type(self.timeout).__name__}",
            )
        elif not 1 <= self.timeout <= 86400:
            set_error(
                "timeout",
                f"timeout must be 1-86400 seconds (24 hours), got {self.timeout}",
            )

        # Validate subagent_timeout
        if isinstance(self.subagent_timeout, bool):
            set_error(
                "subagent_timeout",
                f"subagent_timeout must be an integer, not boolean (got {self.subagent_timeout})",
            )
        elif not isinstance(self.subagent_timeout, int):
            set_error(
                "subagent_timeout",
                f"subagent_timeout must be an integer, got {type(self.subagent_timeout).__name__}",
            )
        elif not 1 <= self.subagent_timeout <= 86400:
            set_error(
                "subagent_timeout",
                f"subagent_timeout must be 1-86400 seconds, got {self.subagent_timeout}",
            )

        # Validate terminal
        from .settings import get_merged_presets

        if not isinstance(self.terminal, str):
            set_error(
                "terminal",
                f"terminal must be a string, got {type(self.terminal).__name__}",
            )
        elif not self.terminal:  # Empty string
            set_error("terminal", "terminal cannot be empty")
        else:
            # 'print' mode shows script content without executing (for debugging)
            # 'here' mode forces running in current terminal (internal/debug)
            merged_presets = get_merged_presets()
            # Resolve old casing (WezTerm→wezterm, Alacritty→alacritty)
            lower_map = {k.lower(): k for k in merged_presets}
            if self.terminal not in merged_presets and self.terminal.lower() in lower_map:
                self.terminal = lower_map[self.terminal.lower()]
            if self.terminal not in ("default", "print", "here") and self.terminal not in merged_presets:
                if "{script}" not in self.terminal:
                    set_error(
                        "terminal",
                        f"terminal must be 'default', preset name, or custom command with {{script}}, "
                        f"got '{self.terminal}'",
                    )

        # Validate tag (only alphanumeric and hyphens - security: prevent log delimiter injection)
        if not isinstance(self.tag, str):
            set_error("tag", f"tag must be a string, got {type(self.tag).__name__}")
        elif self.tag and not re.match(r"^[a-zA-Z0-9-]+$", self.tag):
            set_error("tag", "tag can only contain letters, numbers, and hyphens")

        # Validate claude_args (must be valid shell-quoted string)
        if not isinstance(self.claude_args, str):
            set_error(
                "claude_args",
                f"claude_args must be a string, got {type(self.claude_args).__name__}",
            )
        elif self.claude_args:
            try:
                # Test if it can be parsed as shell args
                shlex.split(self.claude_args)
            except ValueError as e:
                set_error("claude_args", f"claude_args contains invalid shell quoting: {e}")

        # Validate gemini_args (must be valid shell-quoted string)
        if not isinstance(self.gemini_args, str):
            set_error(
                "gemini_args",
                f"gemini_args must be a string, got {type(self.gemini_args).__name__}",
            )
        elif self.gemini_args:
            try:
                # Test if it can be parsed as shell args
                shlex.split(self.gemini_args)
            except ValueError as e:
                set_error("gemini_args", f"gemini_args contains invalid shell quoting: {e}")

        # Validate codex_args (must be valid shell-quoted string)
        if not isinstance(self.codex_args, str):
            set_error(
                "codex_args",
                f"codex_args must be a string, got {type(self.codex_args).__name__}",
            )
        elif self.codex_args:
            try:
                # Test if it can be parsed as shell args
                shlex.split(self.codex_args)
            except ValueError as e:
                set_error("codex_args", f"codex_args contains invalid shell quoting: {e}")

        # Validate codex_sandbox_mode (must be one of valid modes)
        valid_sandbox_modes = ("workspace", "untrusted", "danger-full-access", "none")
        if not isinstance(self.codex_sandbox_mode, str):
            set_error(
                "codex_sandbox_mode",
                f"codex_sandbox_mode must be a string, got {type(self.codex_sandbox_mode).__name__}",
            )
        elif self.codex_sandbox_mode not in valid_sandbox_modes:
            set_error(
                "codex_sandbox_mode",
                f"codex_sandbox_mode must be one of {valid_sandbox_modes}, got '{self.codex_sandbox_mode}'",
            )

        # Validate relay (optional string - MQTT broker URL)
        if not isinstance(self.relay, str):
            set_error("relay", f"relay must be a string, got {type(self.relay).__name__}")

        # Validate relay_id (optional string - UUID for topic namespacing)
        if not isinstance(self.relay_id, str):
            set_error("relay_id", f"relay_id must be a string, got {type(self.relay_id).__name__}")

        # Validate relay_token (optional string)
        if not isinstance(self.relay_token, str):
            set_error(
                "relay_token",
                f"relay_token must be a string, got {type(self.relay_token).__name__}",
            )

        # Validate relay_enabled (boolean)
        if not isinstance(self.relay_enabled, bool):
            set_error(
                "relay_enabled",
                f"relay_enabled must be a boolean, got {type(self.relay_enabled).__name__}",
            )

        # Validate auto_approve (boolean)
        if not isinstance(self.auto_approve, bool):
            set_error(
                "auto_approve",
                f"auto_approve must be a boolean, got {type(self.auto_approve).__name__}",
            )

        # Validate auto_subscribe (comma-separated preset names)
        if not isinstance(self.auto_subscribe, str):
            set_error(
                "auto_subscribe",
                f"auto_subscribe must be a string, got {type(self.auto_subscribe).__name__}",
            )
        elif self.auto_subscribe:
            # Check each preset name is alphanumeric/underscore (no SQL injection)
            for preset in self.auto_subscribe.split(","):
                preset = preset.strip()
                if preset and not re.match(r"^[a-zA-Z0-9_]+$", preset):
                    set_error(
                        "auto_subscribe",
                        f"auto_subscribe preset '{preset}' contains invalid characters (alphanumeric/underscore only)",
                    )

        return errors

    @classmethod
    def load(cls) -> "HcomConfig":
        """Load config with precedence: env var → config.toml → defaults"""
        toml_path = hcom_path(CONFIG_TOML, ensure_parent=True)

        if not toml_path.exists():
            # Try migration from config.env
            config_env_path = hcom_path(CONFIG_FILE)
            if config_env_path.exists():
                _migrate_config_env_to_toml()
            # If still missing (migration failed or no old file), write defaults
            if not toml_path.exists():
                _write_default_config()

        # Parse config.toml
        file_config = load_toml_config(toml_path) if toml_path.exists() else {}

        def get_var(field: str) -> Any | None:
            """Get variable with precedence: env → file. Returns None if not set."""
            env_key = _FIELD_TO_ENV.get(field)
            # Relay fields are file-only (no env override)
            if env_key and field not in _RELAY_FIELDS and env_key in os.environ:
                return os.environ[env_key]
            if field in file_config:
                return file_config[field]
            return None

        data: dict[str, Any] = {}

        # Load integer fields
        for int_field in ("timeout", "subagent_timeout"):
            val = get_var(int_field)
            if val is not None:
                if isinstance(val, int) and not isinstance(val, bool):
                    data[int_field] = val
                elif isinstance(val, str) and val != "":
                    try:
                        data[int_field] = int(val)
                    except (ValueError, TypeError):
                        from .log import log_warn
                        env_key = _FIELD_TO_ENV[int_field]
                        log_warn("config", f"invalid_{int_field}",
                                f"{env_key}='{val}' is not a valid integer, using default")

        # Load string fields
        for str_field in (
            "terminal", "hints", "notes", "tag",
            "claude_args", "gemini_args", "codex_args",
            "codex_sandbox_mode", "gemini_system_prompt", "codex_system_prompt",
            "auto_subscribe", "name_export",
        ):
            val = get_var(str_field)
            if val is not None:
                str_val = str(val)
                # terminal and codex_sandbox_mode: skip empty (use default)
                if str_field in ("terminal", "codex_sandbox_mode") and str_val == "":
                    continue
                # Normalize legacy sandbox mode value
                if str_field == "codex_sandbox_mode" and str_val == "full-auto":
                    str_val = "danger-full-access"
                data[str_field] = str_val

        # Load boolean fields
        for bool_field in ("relay_enabled", "auto_approve"):
            val = get_var(bool_field)
            if val is not None:
                if isinstance(val, bool):
                    data[bool_field] = val
                elif isinstance(val, str):
                    data[bool_field] = val not in ("0", "false", "False", "no", "off", "")

        # Load relay string fields (file-only, already handled by get_var)
        for relay_field in ("relay", "relay_id", "relay_token"):
            val = get_var(relay_field)
            if val is not None:
                data[relay_field] = str(val)

        return cls(**data)  # Validation happens in __post_init__


def get_config_sources() -> dict[str, str]:
    """Get source of each config value: 'env', 'toml', or 'default'."""
    toml_path = hcom_path(CONFIG_TOML, ensure_parent=True)
    file_config = load_toml_config(toml_path) if toml_path.exists() else {}

    sources = {}
    for env_key in KNOWN_CONFIG_KEYS:
        field = _ENV_TO_FIELD.get(env_key)
        if not field:
            sources[env_key] = "default"
            continue
        # Relay fields are file-only
        if field not in _RELAY_FIELDS and env_key in os.environ:
            sources[env_key] = "env"
        elif field in file_config:
            sources[env_key] = "toml"
        else:
            sources[env_key] = "default"
    return sources


# ==================== Config Snapshot ====================


@dataclass
class ConfigSnapshot:
    core: HcomConfig
    extras: dict[str, str]
    values: dict[str, str]


# ==================== Config Conversion ====================


def hcom_config_to_dict(config: HcomConfig) -> dict[str, str]:
    """Convert HcomConfig to string dict for persistence/display."""
    return {
        "HCOM_TIMEOUT": str(config.timeout),
        "HCOM_SUBAGENT_TIMEOUT": str(config.subagent_timeout),
        "HCOM_TERMINAL": config.terminal,
        "HCOM_HINTS": config.hints,
        "HCOM_NOTES": config.notes,
        "HCOM_TAG": config.tag,
        "HCOM_CLAUDE_ARGS": config.claude_args,
        "HCOM_GEMINI_ARGS": config.gemini_args,
        "HCOM_CODEX_ARGS": config.codex_args,
        "HCOM_CODEX_SANDBOX_MODE": config.codex_sandbox_mode,
        "HCOM_GEMINI_SYSTEM_PROMPT": config.gemini_system_prompt,
        "HCOM_CODEX_SYSTEM_PROMPT": config.codex_system_prompt,
        "HCOM_RELAY": config.relay,
        "HCOM_RELAY_ID": config.relay_id,
        "HCOM_RELAY_TOKEN": config.relay_token,
        "HCOM_RELAY_ENABLED": "1" if config.relay_enabled else "0",
        "HCOM_AUTO_APPROVE": "1" if config.auto_approve else "0",
        "HCOM_AUTO_SUBSCRIBE": config.auto_subscribe,
        "HCOM_NAME_EXPORT": config.name_export,
    }


def dict_to_hcom_config(data: dict[str, str]) -> HcomConfig:
    """Convert string dict (HCOM_* keys) into validated HcomConfig."""
    errors: dict[str, str] = {}
    kwargs: dict[str, Any] = {}

    timeout_raw = data.get("HCOM_TIMEOUT")
    if timeout_raw is not None:
        stripped = timeout_raw.strip()
        if stripped:
            try:
                kwargs["timeout"] = int(stripped)
            except ValueError:
                errors["timeout"] = f"timeout must be an integer, got '{timeout_raw}'"
        else:
            errors["timeout"] = "timeout cannot be empty (must be 1-86400 seconds)"

    subagent_raw = data.get("HCOM_SUBAGENT_TIMEOUT")
    if subagent_raw is not None:
        stripped = subagent_raw.strip()
        if stripped:
            try:
                kwargs["subagent_timeout"] = int(stripped)
            except ValueError:
                errors["subagent_timeout"] = f"subagent_timeout must be an integer, got '{subagent_raw}'"
        else:
            errors["subagent_timeout"] = "subagent_timeout cannot be empty (must be positive integer)"

    terminal_val = data.get("HCOM_TERMINAL")
    if terminal_val is not None:
        stripped = terminal_val.strip()
        if stripped:
            kwargs["terminal"] = stripped
        else:
            errors["terminal"] = "terminal cannot be empty (must be: default, preset name, or custom command)"

    # Optional fields - allow empty strings
    if "HCOM_HINTS" in data:
        kwargs["hints"] = data["HCOM_HINTS"]
    if "HCOM_NOTES" in data:
        kwargs["notes"] = data["HCOM_NOTES"]
    if "HCOM_TAG" in data:
        kwargs["tag"] = data["HCOM_TAG"]
    if "HCOM_CLAUDE_ARGS" in data:
        kwargs["claude_args"] = data["HCOM_CLAUDE_ARGS"]
    if "HCOM_GEMINI_ARGS" in data:
        kwargs["gemini_args"] = data["HCOM_GEMINI_ARGS"]
    if "HCOM_CODEX_ARGS" in data:
        kwargs["codex_args"] = data["HCOM_CODEX_ARGS"]
    if "HCOM_CODEX_SANDBOX_MODE" in data:
        val = data["HCOM_CODEX_SANDBOX_MODE"]
        kwargs["codex_sandbox_mode"] = "danger-full-access" if val == "full-auto" else val
    if "HCOM_GEMINI_SYSTEM_PROMPT" in data:
        kwargs["gemini_system_prompt"] = data["HCOM_GEMINI_SYSTEM_PROMPT"]
    if "HCOM_CODEX_SYSTEM_PROMPT" in data:
        kwargs["codex_system_prompt"] = data["HCOM_CODEX_SYSTEM_PROMPT"]
    if "HCOM_RELAY" in data:
        kwargs["relay"] = data["HCOM_RELAY"]
    if "HCOM_RELAY_ID" in data:
        kwargs["relay_id"] = data["HCOM_RELAY_ID"]
    if "HCOM_RELAY_TOKEN" in data:
        kwargs["relay_token"] = data["HCOM_RELAY_TOKEN"]
    if "HCOM_RELAY_ENABLED" in data:
        kwargs["relay_enabled"] = data["HCOM_RELAY_ENABLED"] not in (
            "0",
            "false",
            "False",
            "no",
            "off",
            "",
        )
    if "HCOM_AUTO_APPROVE" in data:
        kwargs["auto_approve"] = data["HCOM_AUTO_APPROVE"] not in (
            "0",
            "false",
            "False",
            "no",
            "off",
            "",
        )
    if "HCOM_AUTO_SUBSCRIBE" in data:
        kwargs["auto_subscribe"] = data["HCOM_AUTO_SUBSCRIBE"]
    if "HCOM_NAME_EXPORT" in data:
        kwargs["name_export"] = data["HCOM_NAME_EXPORT"]
    if errors:
        raise HcomConfigError(errors)

    return HcomConfig(**kwargs)


# ==================== Config Snapshot I/O ====================


def load_config_snapshot() -> ConfigSnapshot:
    """Load config.toml + env into structured snapshot (file contents only, no env overrides)."""
    toml_path = hcom_path(CONFIG_TOML, ensure_parent=True)
    if not toml_path.exists():
        # Try migration, then write defaults
        config_env_path = hcom_path(CONFIG_FILE)
        if config_env_path.exists():
            _migrate_config_env_to_toml()
        if not toml_path.exists():
            _write_default_config()

    # Load TOML config values
    file_config = load_toml_config(toml_path) if toml_path.exists() else {}

    # Build raw_core dict (HCOM_* keys) from TOML values
    defaults = _get_default_known_values()
    raw_core: dict[str, str] = {}
    for env_key in KNOWN_CONFIG_KEYS:
        field = _ENV_TO_FIELD.get(env_key)
        if field and field in file_config:
            val = file_config[field]
            if isinstance(val, bool):
                raw_core[env_key] = "1" if val else "0"
            else:
                raw_core[env_key] = str(val)
        else:
            raw_core[env_key] = defaults.get(env_key, "")

    # Load env extras (non-HCOM passthrough vars)
    env_path = hcom_path(ENV_FILE)
    extras = load_env_extras(env_path)

    try:
        core = dict_to_hcom_config(raw_core)
    except HcomConfigError as exc:
        core = HcomConfig()
        if exc.errors:
            print(exc, file=sys.stderr)

    core_values = hcom_config_to_dict(core)
    # Preserve raw strings for display when they differ from validated values.
    for key, raw_value in raw_core.items():
        if raw_value != "" and raw_value != core_values.get(key, ""):
            core_values[key] = raw_value

    return ConfigSnapshot(core=core, extras=extras, values=core_values)


def save_config_snapshot(snapshot: ConfigSnapshot) -> None:
    """Write snapshot: core → config.toml, extras → env file."""
    # Load existing presets to preserve them
    toml_path = hcom_path(CONFIG_TOML, ensure_parent=True)
    existing_presets = _load_toml_presets(toml_path) if toml_path.exists() else {}

    save_toml_config(snapshot.core, presets=existing_presets or None)

    # Write env extras
    _save_env_file(snapshot.extras)


def save_config(core: HcomConfig, extras: dict[str, str]) -> None:
    """Convenience helper for writing canonical config."""
    snapshot = ConfigSnapshot(core=core, extras=extras, values=hcom_config_to_dict(core))
    save_config_snapshot(snapshot)


# ==================== Config File Writing ====================


def _write_default_config() -> None:
    """Write default config.toml + env file."""
    try:
        save_toml_config(HcomConfig())
        # Write env file with placeholders
        _save_env_file({})
    except Exception as exc:
        from .log import log_warn
        log_warn("config", "default_config_write_failed", str(exc))


# ==================== Global Config Cache ====================

_config_cache: HcomConfig | None = None
_config_cache_lock = threading.Lock()


def get_config() -> HcomConfig:
    """Get cached config, loading if needed (thread-safe with double-checked locking)"""
    global _config_cache

    # First check without lock (fast path for already-loaded config)
    if _config_cache is not None:
        return _config_cache

    # Acquire lock for initialization
    with _config_cache_lock:
        # Double-check inside lock (another thread may have initialized while we waited)
        if _config_cache is not None:
            return _config_cache

        # Detect if running as hook handler (called via 'hcom pre', 'hcom post', etc.)
        is_hook_context = len(sys.argv) >= 2 and sys.argv[1] in (
            "pre",
            "post",
            "sessionstart",
            "userpromptsubmit",
            "sessionend",
            "subagent-stop",
            "poll",
            "notify",
        )

        try:
            _config_cache = HcomConfig.load()
        except ValueError:
            if is_hook_context:
                _config_cache = HcomConfig()
            else:
                raise

        return _config_cache


def reload_config() -> HcomConfig:
    """Clear cached config so next access reflects latest file/env values (thread-safe)."""
    global _config_cache
    with _config_cache_lock:
        _config_cache = None
    # Also clear settings/preset cache so new presets are picked up
    from .settings import invalidate_settings_cache
    invalidate_settings_cache()
    return get_config()
