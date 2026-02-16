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


def looks_like_flag(
    token_lower: str,
    *,
    boolean_flags: frozenset[str],
    value_flags: frozenset[str],
    value_flag_prefixes: frozenset[str],
    optional_value_flags: frozenset[str] = frozenset(),
    flag_aliases: Mapping[str, str] | None = None,
    optional_value_flag_prefixes: frozenset[str] = frozenset(),
    canonical_prefixes: Mapping[str, str] | None = None,
    extra_flags: frozenset[str] = frozenset(),
) -> bool:
    """Check if token looks like a flag (not a value).

    Used to detect when a value flag is missing its value (next token is another flag).
    Takes tool-specific flag configuration as parameters.

    Args:
        token_lower: Lowercase token to check
        boolean_flags: Known boolean flags (e.g., '--verbose')
        value_flags: Known value flags (e.g., '--model')
        value_flag_prefixes: Prefixes for --flag=value syntax (e.g., '--model=')
        optional_value_flags: Flags with optional values (e.g., '--resume')
        flag_aliases: Short-to-long flag mappings (e.g., {'-m': '--model'})
        optional_value_flag_prefixes: Prefixes for optional value flags (e.g., '--resume=')
        canonical_prefixes: Alias prefixes to canonical form (e.g., {'--allowedtools=': '--allowedTools'})
        extra_flags: Additional flags to check (e.g., background switches)

    Returns:
        True if token looks like a flag, False otherwise
    """
    if token_lower in extra_flags:
        return True
    if token_lower in boolean_flags:
        return True
    if token_lower in value_flags:
        return True
    if token_lower in optional_value_flags:
        return True
    if flag_aliases and token_lower in flag_aliases:
        return True
    if token_lower == "--":
        return True
    if any(token_lower.startswith(prefix) for prefix in optional_value_flag_prefixes):
        return True
    if any(token_lower.startswith(prefix) for prefix in value_flag_prefixes):
        return True
    if canonical_prefixes and any(token_lower.startswith(prefix) for prefix in canonical_prefixes):
        return True
    return False


def set_positional(
    tokens: Sequence[str],
    value: str,
    positional_indexes: Sequence[int],
) -> TokenList:
    """Set or replace the first positional argument.

    If a positional exists, replaces it. Otherwise appends the value.

    Args:
        tokens: Token list to modify
        value: New positional value
        positional_indexes: Indexes of positional tokens in the list

    Returns:
        Modified token list
    """
    tokens_list: TokenList = list(tokens)
    if positional_indexes:
        # Replace first positional
        tokens_list[positional_indexes[0]] = value
        return tokens_list
    # No existing positional, append
    tokens_list.append(value)
    return tokens_list


def remove_positional(
    tokens: Sequence[str],
    positional_indexes: Sequence[int],
) -> TokenList:
    """Remove the first positional argument.

    Args:
        tokens: Token list to modify
        positional_indexes: Indexes of positional tokens

    Returns:
        Token list with first positional removed
    """
    if not positional_indexes:
        return list(tokens)  # Nothing to remove
    idx: int = positional_indexes[0]
    return list(tokens[:idx]) + list(tokens[idx + 1 :])
