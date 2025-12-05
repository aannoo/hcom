"""Configuration management - central config system used by all modules."""
from __future__ import annotations

import os
import sys
import re
import shlex
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .paths import hcom_path, atomic_write, CONFIG_FILE
from ..shared import (
    parse_env_file, parse_env_value, format_env_value,
    DEFAULT_CONFIG_HEADER, DEFAULT_CONFIG_DEFAULTS,
)

# ==================== Config Constants ====================

# Parse header defaults (ANTHROPIC_MODEL, CLAUDE_CODE_SUBAGENT_MODEL, etc.)
_HEADER_COMMENT_LINES: list[str] = []
_HEADER_DEFAULT_EXTRAS: dict[str, str] = {}
_header_data_started = False
for _line in DEFAULT_CONFIG_HEADER:
    stripped = _line.strip()
    if not _header_data_started and (not stripped or stripped.startswith('#')):
        _HEADER_COMMENT_LINES.append(_line)
        continue
    if not _header_data_started:
        _header_data_started = True
    if _header_data_started and '=' in _line:
        key, _, value = _line.partition('=')
        _HEADER_DEFAULT_EXTRAS[key.strip()] = parse_env_value(value)

# Parse known HCOM_* config keys and defaults
KNOWN_CONFIG_KEYS: list[str] = []
DEFAULT_KNOWN_VALUES: dict[str, str] = {}
for _entry in DEFAULT_CONFIG_DEFAULTS:
    if '=' not in _entry:
        continue
    key, _, value = _entry.partition('=')
    key = key.strip()
    KNOWN_CONFIG_KEYS.append(key)
    DEFAULT_KNOWN_VALUES[key] = parse_env_value(value)


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
    """HCOM configuration with validation. Load priority: env → file → defaults"""
    timeout: int = 1800
    subagent_timeout: int = 30
    terminal: str = 'new'
    hints: str = ''
    tag: str = ''
    claude_args: str = ''
    relay: str = ''
    relay_token: str = ''
    relay_enabled: bool = True

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
            set_error('timeout', f"timeout must be an integer, not boolean (got {self.timeout})")
        elif not isinstance(self.timeout, int):
            set_error('timeout', f"timeout must be an integer, got {type(self.timeout).__name__}")
        elif not 1 <= self.timeout <= 86400:
            set_error('timeout', f"timeout must be 1-86400 seconds (24 hours), got {self.timeout}")

        # Validate subagent_timeout
        if isinstance(self.subagent_timeout, bool):
            set_error('subagent_timeout', f"subagent_timeout must be an integer, not boolean (got {self.subagent_timeout})")
        elif not isinstance(self.subagent_timeout, int):
            set_error('subagent_timeout', f"subagent_timeout must be an integer, got {type(self.subagent_timeout).__name__}")
        elif not 1 <= self.subagent_timeout <= 86400:
            set_error('subagent_timeout', f"subagent_timeout must be 1-86400 seconds, got {self.subagent_timeout}")

        # Validate terminal
        if not isinstance(self.terminal, str):
            set_error('terminal', f"terminal must be a string, got {type(self.terminal).__name__}")
        elif not self.terminal:  # Empty string
            set_error('terminal', "terminal cannot be empty")
        else:
            valid_modes = ('new', 'here', 'print')
            if self.terminal not in valid_modes:
                if '{script}' not in self.terminal:
                    set_error(
                        'terminal',
                        f"terminal must be one of {valid_modes} or custom command with {{script}}, "
                        f"got '{self.terminal}'"
                    )

        # Validate tag (only alphanumeric and hyphens - security: prevent log delimiter injection)
        if not isinstance(self.tag, str):
            set_error('tag', f"tag must be a string, got {type(self.tag).__name__}")
        elif self.tag and not re.match(r'^[a-zA-Z0-9-]+$', self.tag):
            set_error('tag', "tag can only contain letters, numbers, and hyphens")

        # Validate claude_args (must be valid shell-quoted string)
        if not isinstance(self.claude_args, str):
            set_error('claude_args', f"claude_args must be a string, got {type(self.claude_args).__name__}")
        elif self.claude_args:
            try:
                # Test if it can be parsed as shell args
                shlex.split(self.claude_args)
            except ValueError as e:
                set_error('claude_args', f"claude_args contains invalid shell quoting: {e}")

        # Validate relay (optional string - URL)
        if not isinstance(self.relay, str):
            set_error('relay', f"relay must be a string, got {type(self.relay).__name__}")

        # Validate relay_token (optional string)
        if not isinstance(self.relay_token, str):
            set_error('relay_token', f"relay_token must be a string, got {type(self.relay_token).__name__}")

        # Validate relay_enabled (boolean)
        if not isinstance(self.relay_enabled, bool):
            set_error('relay_enabled', f"relay_enabled must be a boolean, got {type(self.relay_enabled).__name__}")

        return errors

    @classmethod
    def load(cls) -> 'HcomConfig':
        """Load config with precedence: env var → file → defaults"""
        # Ensure config file exists
        config_path = hcom_path(CONFIG_FILE, ensure_parent=True)
        created_config = False
        if not config_path.exists():
            _write_default_config(config_path)
            created_config = True

        # Warn once if legacy config.json still exists when creating config.env
        legacy_config = hcom_path('config.json')
        if created_config and legacy_config.exists():
            print(
                "Error: Found legacy ~/.hcom/config.json; new config file is: ~/.hcom/config.env.",
                file=sys.stderr,
            )

        # Parse config file once
        file_config = parse_env_file(config_path) if config_path.exists() else {}

        def get_var(key: str) -> str | None:
            """Get variable with precedence: env → file"""
            if key in os.environ:
                return os.environ[key]
            if key in file_config:
                return file_config[key]
            return None

        data = {}

        # Load timeout (requires int conversion)
        timeout_str = get_var('HCOM_TIMEOUT')
        if timeout_str is not None and timeout_str != "":
            try:
                data['timeout'] = int(timeout_str)
            except (ValueError, TypeError):
                print(f"Warning: HCOM_TIMEOUT='{timeout_str}' is not a valid integer, using default", file=sys.stderr)

        # Load subagent_timeout (requires int conversion)
        subagent_timeout_str = get_var('HCOM_SUBAGENT_TIMEOUT')
        if subagent_timeout_str is not None and subagent_timeout_str != "":
            try:
                data['subagent_timeout'] = int(subagent_timeout_str)
            except (ValueError, TypeError):
                print(f"Warning: HCOM_SUBAGENT_TIMEOUT='{subagent_timeout_str}' is not a valid integer, using default", file=sys.stderr)

        # Load string values
        terminal = get_var('HCOM_TERMINAL')
        if terminal is not None:  # Empty string will fail validation
            data['terminal'] = terminal
        hints = get_var('HCOM_HINTS')
        if hints is not None:  # Allow empty string for hints (valid value)
            data['hints'] = hints
        tag = get_var('HCOM_TAG')
        if tag is not None:  # Allow empty string for tag (valid value)
            data['tag'] = tag
        claude_args = get_var('HCOM_CLAUDE_ARGS')
        if claude_args is not None:  # Allow empty string for claude_args (valid value)
            data['claude_args'] = claude_args

        # Load relay string values
        relay = get_var('HCOM_RELAY')
        if relay is not None:  # Allow empty string for relay (valid value - means disabled)
            data['relay'] = relay
        relay_token = get_var('HCOM_RELAY_TOKEN')
        if relay_token is not None:  # Allow empty string for token (valid value)
            data['relay_token'] = relay_token

        # Load relay_enabled (boolean - "0" or "1", default True)
        relay_enabled_str = get_var('HCOM_RELAY_ENABLED')
        if relay_enabled_str is not None:
            data['relay_enabled'] = relay_enabled_str not in ('0', 'false', 'False', 'no', 'off')

        return cls(**data)  # Validation happens in __post_init__


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
        'HCOM_TIMEOUT': str(config.timeout),
        'HCOM_SUBAGENT_TIMEOUT': str(config.subagent_timeout),
        'HCOM_TERMINAL': config.terminal,
        'HCOM_HINTS': config.hints,
        'HCOM_TAG': config.tag,
        'HCOM_CLAUDE_ARGS': config.claude_args,
        'HCOM_RELAY': config.relay,
        'HCOM_RELAY_TOKEN': config.relay_token,
        'HCOM_RELAY_ENABLED': '1' if config.relay_enabled else '0',
    }


def dict_to_hcom_config(data: dict[str, str]) -> HcomConfig:
    """Convert string dict (HCOM_* keys) into validated HcomConfig."""
    errors: dict[str, str] = {}
    kwargs: dict[str, Any] = {}

    timeout_raw = data.get('HCOM_TIMEOUT')
    if timeout_raw is not None:
        stripped = timeout_raw.strip()
        if stripped:
            try:
                kwargs['timeout'] = int(stripped)
            except ValueError:
                errors['timeout'] = f"timeout must be an integer, got '{timeout_raw}'"
        else:
            # Explicit empty string is an error (can't be blank)
            errors['timeout'] = 'timeout cannot be empty (must be 1-86400 seconds)'

    subagent_raw = data.get('HCOM_SUBAGENT_TIMEOUT')
    if subagent_raw is not None:
        stripped = subagent_raw.strip()
        if stripped:
            try:
                kwargs['subagent_timeout'] = int(stripped)
            except ValueError:
                errors['subagent_timeout'] = f"subagent_timeout must be an integer, got '{subagent_raw}'"
        else:
            # Explicit empty string is an error (can't be blank)
            errors['subagent_timeout'] = 'subagent_timeout cannot be empty (must be positive integer)'

    terminal_val = data.get('HCOM_TERMINAL')
    if terminal_val is not None:
        stripped = terminal_val.strip()
        if stripped:
            kwargs['terminal'] = stripped
        else:
            # Explicit empty string is an error (can't be blank)
            errors['terminal'] = 'terminal cannot be empty (must be: new, here, print, or custom command)'

    # Optional fields - allow empty strings
    if 'HCOM_HINTS' in data:
        kwargs['hints'] = data['HCOM_HINTS']
    if 'HCOM_TAG' in data:
        kwargs['tag'] = data['HCOM_TAG']
    if 'HCOM_CLAUDE_ARGS' in data:
        kwargs['claude_args'] = data['HCOM_CLAUDE_ARGS']
    if 'HCOM_RELAY' in data:
        kwargs['relay'] = data['HCOM_RELAY']
    if 'HCOM_RELAY_TOKEN' in data:
        kwargs['relay_token'] = data['HCOM_RELAY_TOKEN']
    if 'HCOM_RELAY_ENABLED' in data:
        kwargs['relay_enabled'] = data['HCOM_RELAY_ENABLED'] not in ('0', 'false', 'False', 'no', 'off')

    if errors:
        raise HcomConfigError(errors)

    return HcomConfig(**kwargs)


# ==================== Config Snapshot I/O ====================

def load_config_snapshot() -> ConfigSnapshot:
    """Load config.env into structured snapshot (file contents only)."""
    config_path = hcom_path(CONFIG_FILE, ensure_parent=True)
    if not config_path.exists():
        _write_default_config(config_path)

    file_values = parse_env_file(config_path)

    extras: dict[str, str] = {k: v for k, v in _HEADER_DEFAULT_EXTRAS.items()}
    raw_core: dict[str, str] = {}

    for key in KNOWN_CONFIG_KEYS:
        if key in file_values:
            raw_core[key] = file_values.pop(key)
        else:
            raw_core[key] = DEFAULT_KNOWN_VALUES.get(key, '')

    for key, value in file_values.items():
        extras[key] = value

    try:
        core = dict_to_hcom_config(raw_core)
    except HcomConfigError as exc:
        core = HcomConfig()
        # Keep raw values so the UI can surface issues; log once for CLI users.
        if exc.errors:
            print(exc, file=sys.stderr)

    core_values = hcom_config_to_dict(core)
    # Preserve raw strings for display when they differ from validated values.
    for key, raw_value in raw_core.items():
        if raw_value != '' and raw_value != core_values.get(key, ''):
            core_values[key] = raw_value

    return ConfigSnapshot(core=core, extras=extras, values=core_values)


def save_config_snapshot(snapshot: ConfigSnapshot) -> None:
    """Write snapshot to config.env in canonical form."""
    config_path = hcom_path(CONFIG_FILE, ensure_parent=True)

    lines: list[str] = list(_HEADER_COMMENT_LINES)
    if lines and lines[-1] != '':
        lines.append('')

    core_values = hcom_config_to_dict(snapshot.core)
    for entry in DEFAULT_CONFIG_DEFAULTS:
        key, _, _ = entry.partition('=')
        key = key.strip()
        value = core_values.get(key, '')
        formatted = format_env_value(value)
        if formatted:
            lines.append(f"{key}={formatted}")
        else:
            lines.append(f"{key}=")

    extras = {**_HEADER_DEFAULT_EXTRAS, **snapshot.extras}
    for key in KNOWN_CONFIG_KEYS:
        extras.pop(key, None)

    if extras:
        if lines and lines[-1] != '':
            lines.append('')
        for key in sorted(extras.keys()):
            value = extras[key]
            formatted = format_env_value(value)
            lines.append(f"{key}={formatted}" if formatted else f"{key}=")

    content = '\n'.join(lines) + '\n'
    atomic_write(config_path, content)


def save_config(core: HcomConfig, extras: dict[str, str]) -> None:
    """Convenience helper for writing canonical config."""
    snapshot = ConfigSnapshot(core=core, extras=extras, values=hcom_config_to_dict(core))
    save_config_snapshot(snapshot)


# ==================== Config File Writing ====================

def _write_default_config(config_path: Path) -> None:
    """Write default config file with documentation"""
    try:
        content = '\n'.join(DEFAULT_CONFIG_HEADER) + '\n' + '\n'.join(DEFAULT_CONFIG_DEFAULTS) + '\n'
        atomic_write(config_path, content)
    except Exception:
        pass


# ==================== Global Config Cache ====================

_config_cache: HcomConfig | None = None


def get_config() -> HcomConfig:
    """Get cached config, loading if needed"""
    global _config_cache
    if _config_cache is None:
        # Detect if running as hook handler (called via 'hcom pre', 'hcom post', etc.)
        is_hook_context = (
            len(sys.argv) >= 2 and
            sys.argv[1] in ('pre', 'post', 'sessionstart', 'userpromptsubmit', 'sessionend', 'subagent-stop', 'poll', 'notify')
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
    """Clear cached config so next access reflects latest file/env values."""
    global _config_cache
    _config_cache = None
    return get_config()
