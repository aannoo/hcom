#!/usr/bin/env python3
"""Common argument parsing infrastructure for CLI tools.

Shared types, base classes, and helper functions used by claude/args.py,
gemini/args.py, and codex/args.py.

Each tool has specific flags and semantics, but shares:
- Token parsing patterns (flags, values, positionals)
- ArgsSpec dataclass structure
- Helper functions for token manipulation
"""

from __future__ import annotations

import shlex
from dataclasses import dataclass
from typing import Iterable, Literal, Mapping, Sequence, TypeAlias

# ==================== Shared Type Aliases ====================

SourceType: TypeAlias = Literal["cli", "env", "none"]
TokenList: TypeAlias = list[str]
TokenTuple: TypeAlias = tuple[str, ...]
FlagValue: TypeAlias = str | list[str]
FlagValuesDict: TypeAlias = dict[str, FlagValue]
FlagValuesMapping: TypeAlias = Mapping[str, FlagValue]


# ==================== Base ArgsSpec ====================


@dataclass(frozen=True)
class BaseArgsSpec:
    """Base class for tool-specific argument specifications.

    Provides common fields and methods shared across all CLI tools.
    Tool-specific subclasses add their own semantic fields (is_background,
    is_headless, is_json, etc.).
    """

    source: SourceType
    raw_tokens: TokenTuple
    clean_tokens: TokenTuple
    positional_tokens: TokenTuple
    positional_indexes: tuple[int, ...]
    flag_values: FlagValuesMapping
    errors: tuple[str, ...] = ()

    def has_flag(
        self,
        names: Iterable[str] | None = None,
        prefixes: Iterable[str] | None = None,
    ) -> bool:
        """Check for user-provided flags (only scans before -- separator).

        Args:
            names: Exact flag names to check (e.g., ['--verbose', '-p'])
            prefixes: Flag prefixes to check (e.g., ['--model='])

        Returns:
            True if any matching flag is found before the -- separator.
        """
        name_set = {n.lower() for n in (names or ())}
        prefix_tuple = tuple(p.lower() for p in (prefixes or ()))

        # Only scan tokens before --
        try:
            dash_idx = self.clean_tokens.index("--")
            tokens_to_scan = self.clean_tokens[:dash_idx]
        except ValueError:
            tokens_to_scan = self.clean_tokens

        for token in tokens_to_scan:
            lower = token.lower()
            if lower in name_set:
                return True
            if any(lower.startswith(prefix) for prefix in prefix_tuple):
                return True
        return False

    def has_errors(self) -> bool:
        """Check if there are any parsing errors."""
        return bool(self.errors)

    def to_env_string(self) -> str:
        """Render tokens into a shell-safe env string."""
        return shlex.join(self.rebuild_tokens())

    def rebuild_tokens(self, include_positionals: bool = True) -> TokenList:
        """Return token list suitable for invoking the CLI tool.

        Must be overridden by subclasses that need different behavior
        (e.g., including subcommands).
        """
        if include_positionals:
            return list(self.clean_tokens)
        else:
            positional_indexes_set: set[int] = set(self.positional_indexes)
            return [t for i, t in enumerate(self.clean_tokens) if i not in positional_indexes_set]


# ==================== Shared Helper Functions ====================


def extract_flag_names_from_tokens(tokens: Sequence[str]) -> set[str]:
    """Extract normalized (lowercase) flag names from token list.

    Used by merge logic to determine which env flags CLI overrides.

    Examples:
        ['--model', 'opus'] -> {'--model'}
        ['--model=opus'] -> {'--model'}
        ['value', '--verbose'] -> {'--verbose'}
    """
    flag_names: set[str] = set()
    for token in tokens:
        flag_name: str | None = extract_flag_name_from_token(token)
        if flag_name:
            flag_names.add(flag_name)
    return flag_names


def extract_flag_name_from_token(token: str) -> str | None:
    """Extract flag name from token, handling --flag=value syntax.

    Returns lowercase flag name (e.g., '--model' from '--model=opus'),
    or None if token is not a flag.

    Examples:
        '--model' -> '--model'
        '--model=opus' -> '--model'
        '-p' -> '-p'
        'value' -> None
    """
    token_lower: str = token.lower()

    if not token_lower.startswith("-"):
        return None

    if "=" in token_lower:
        return token_lower.split("=")[0]

    return token_lower


def deduplicate_boolean_flags(tokens: Sequence[str], boolean_flags: frozenset[str]) -> TokenList:
    """Remove duplicate boolean flags, keeping first occurrence.

    Only deduplicates known boolean flags from the provided set.
    Unknown flags and value flags are left as-is (CLI handles them).

    Args:
        tokens: Token list to process
        boolean_flags: Set of known boolean flag names (lowercase)

    Returns:
        Token list with duplicate boolean flags removed
    """
    seen_flags: set[str] = set()
    result: TokenList = []

    for token in tokens:
        token_lower: str = token.lower()
        if token_lower in boolean_flags:
            if token_lower in seen_flags:
                continue
            seen_flags.add(token_lower)
        result.append(token)

    return result


def toggle_flag(tokens: Sequence[str], flag: str, desired: bool) -> TokenList:
    """Add or remove a boolean flag from token list.

    If desired=True, ensures flag is present (prepended).
    If desired=False, removes all occurrences.

    Args:
        tokens: Token list to modify
        flag: Flag name to toggle (e.g., '--verbose')
        desired: True to add, False to remove

    Returns:
        Modified token list
    """
    tokens_list: TokenList = list(tokens)
    flag_lower: str = flag.lower()

    # Remove existing occurrences
    filtered: TokenList = [t for t in tokens_list if t.lower() != flag_lower]

    if desired:
        return [flag] + filtered
    return filtered


def set_value_flag(tokens: Sequence[str], flag: str, value: str) -> TokenList:
    """Set a value flag, replacing any existing occurrence.

    Handles both --flag value and --flag=value forms.

    Args:
        tokens: Token list to modify
        flag: Flag name (e.g., '--model')
        value: New value to set

    Returns:
        Modified token list with flag set to new value
    """
    tokens_list: TokenList = list(tokens)
    flag_lower: str = flag.lower()

    # Remove existing occurrences (both --flag value and --flag=value forms)
    result: TokenList = []
    skip_next: bool = False
    for token in tokens_list:
        if skip_next:
            skip_next = False
            continue
        token_lower: str = token.lower()
        if token_lower == flag_lower:
            # Skip this and the next token (the value)
            skip_next = True
            continue
        if token_lower.startswith(flag_lower + "="):
            continue
        result.append(token)

    # Add the new value
    result.extend([flag, value])
    return result


def remove_flag_with_value(tokens: Sequence[str], flag: str) -> TokenList:
    """Remove all occurrences of a flag and its value.

    Args:
        tokens: Token list to modify
        flag: Flag name to remove (e.g., '--model')

    Returns:
        Token list with flag and its value removed
    """
    tokens_list: TokenList = list(tokens)
    flag_lower: str = flag.lower()

    result: TokenList = []
    skip_next: bool = False
    for token in tokens_list:
        if skip_next:
            skip_next = False
            continue
        token_lower: str = token.lower()
        if token_lower == flag_lower:
            skip_next = True
            continue
        if token_lower.startswith(flag_lower + "="):
            continue
        result.append(token)

    return result


def split_env_tokens(env_value: str) -> TokenList:
    """Split shell-quoted environment variable into tokens.

    Args:
        env_value: Shell-quoted string (e.g., '--model opus --verbose')

    Returns:
        List of tokens

    Raises:
        ValueError: If the string has unbalanced quotes
    """
    return shlex.split(env_value)
