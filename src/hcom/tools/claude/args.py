#!/usr/bin/env python3
"""Claude CLI argument parsing and validation.

Parses Claude Code CLI arguments with full support for:
- Flag aliases (--allowed-tools, --allowedtools, --allowedTools)
- Boolean flags (--verbose, -p, etc.)
- Value flags (--model opus, --model=opus)
- Optional value flags (--resume, --resume=session-id)
- Positional arguments (prompt text)
- The -- separator for literal arguments

The parser produces ClaudeArgsSpec which can be:
- Queried for specific flags (has_flag, get_flag_value)
- Modified immutably (update method)
- Merged with other specs (merge_claude_args)
- Serialized back to tokens (rebuild_tokens, to_env_string)
"""

from __future__ import annotations

import difflib
import shlex
from dataclasses import dataclass
from typing import Final, Literal, Mapping, Sequence, TypeAlias

from ..args_common import (
    BaseArgsSpec,
    SourceType,
    TokenList,
    TokenTuple,
    extract_flag_names_from_tokens as _extract_flag_names_from_tokens,
    extract_flag_name_from_token as _extract_flag_name_from_token,
    deduplicate_boolean_flags as _deduplicate_boolean_flags_base,
    looks_like_flag as _looks_like_flag_base,
    set_positional as _set_positional,
    remove_positional as _remove_positional,
)

# Type aliases (Claude-specific)
CanonicalFlag: TypeAlias = Literal["--model", "--allowedTools", "--disallowedTools"]
FlagValuesDict: TypeAlias = dict[str, str]  # Note: using str keys for compatibility with base

# ==================== Flag Classification ====================
# Flags are categorized by their behavior:
# - Boolean flags: standalone, no value (--verbose)
# - Value flags: require following value (--model opus)
# - Optional value flags: value is optional (--resume, --resume=id)
# - Canonical flags: multiple spellings map to one form (--allowedTools)

# All flag keys stored in lowercase (aside from short-form switches) for comparisons;
# values use canonical casing when recorded.
_FLAG_ALIASES: Final[Mapping[str, CanonicalFlag]] = {
    "--model": "--model",
    "--allowedtools": "--allowedTools",
    "--allowed-tools": "--allowedTools",
    "--disallowedtools": "--disallowedTools",
    "--disallowed-tools": "--disallowedTools",
}

_CANONICAL_PREFIXES: Final[Mapping[str, CanonicalFlag]] = {
    "--model=": "--model",
    "--allowedtools=": "--allowedTools",
    "--allowed-tools=": "--allowedTools",
    "--disallowedtools=": "--disallowedTools",
    "--disallowed-tools=": "--disallowedTools",
}

# Background switches: NOT in _BOOLEAN_FLAGS to enable special handling.
# has_flag() detects them by scanning clean_tokens directly, so they remain
# discoverable via spec.has_flag(['-p']) or spec.has_flag(['--print']).
_BACKGROUND_SWITCHES: Final[frozenset[str]] = frozenset({"-p", "--print"})
_FORK_SWITCHES: Final[frozenset[str]] = frozenset({"--fork-session"})
_BOOLEAN_FLAGS: Final[frozenset[str]] = frozenset(
    {
        "--verbose",
        "--continue",
        "-c",
        "--dangerously-skip-permissions",
        "--include-partial-messages",
        "--allow-dangerously-skip-permissions",
        "--replay-user-messages",
        "--mcp-debug",
        # --fork-session handled by _FORK_SWITCHES (sets is_fork)
        "--ide",
        "--strict-mcp-config",
        "--no-session-persistence",
        "--disable-slash-commands",
        "--chrome",
        "--no-chrome",
        "--init",
        "--init-only",
        "--maintenance",
        "-v",
        "--version",
        "-h",
        "--help",
    }
)

# Flags with optional values (lowercase).
_OPTIONAL_VALUE_FLAGS: Final[frozenset[str]] = frozenset(
    {
        "--resume",
        "-r",
        "--debug",
        "-d",
        "--teleport",
        "--from-pr",
    }
)

_OPTIONAL_VALUE_FLAG_PREFIXES: Final[frozenset[str]] = frozenset(
    {
        "--resume=",
        "-r=",
        "--debug=",
        "-d=",
        "--teleport=",
        "--from-pr=",
    }
)

_OPTIONAL_ALIAS_GROUPS: Final[tuple[frozenset[str], ...]] = (
    frozenset({"--resume", "-r"}),
    frozenset({"--debug", "-d"}),
)
_OPTIONAL_ALIAS_LOOKUP: Final[Mapping[str, set[str]]] = {
    alias: set(group) for group in _OPTIONAL_ALIAS_GROUPS for alias in group
}

# Flags that require a following value (lowercase).
_VALUE_FLAGS: Final[frozenset[str]] = frozenset(
    {
        "--add-dir",
        "--agent",
        "--agents",
        "--allowed-tools",
        "--allowedtools",
        "--append-system-prompt",
        "--append-system-prompt-file",
        "--betas",
        "--debug-file",
        "--disallowedtools",
        "--disallowed-tools",
        "--fallback-model",
        "--file",
        "--input-format",
        "--json-schema",
        "--max-budget-usd",
        "--max-turns",
        "--mcp-config",
        "--model",
        "--output-format",
        "--permission-mode",
        "--permission-prompt-tool",
        "--plugin-dir",
        "--remote",
        "--session-id",
        "--setting-sources",
        "--settings",
        "--system-prompt",
        "--system-prompt-file",
        "--teammate-mode",
        "--tools",
    }
)

_VALUE_FLAG_PREFIXES: Final[frozenset[str]] = frozenset(
    {
        "--add-dir=",
        "--agent=",
        "--agents=",
        "--allowedtools=",
        "--allowed-tools=",
        "--append-system-prompt=",
        "--append-system-prompt-file=",
        "--betas=",
        "--debug-file=",
        "--disallowedtools=",
        "--disallowed-tools=",
        "--fallback-model=",
        "--file=",
        "--input-format=",
        "--json-schema=",
        "--max-budget-usd=",
        "--max-turns=",
        "--mcp-config=",
        "--model=",
        "--output-format=",
        "--permission-mode=",
        "--permission-prompt-tool=",
        "--plugin-dir=",
        "--remote=",
        "--session-id=",
        "--setting-sources=",
        "--settings=",
        "--system-prompt=",
        "--system-prompt-file=",
        "--teammate-mode=",
        "--tools=",
    }
)

_KNOWN_CLAUDE_FLAGS: Final[list[str]] = sorted(
    {
        *_BACKGROUND_SWITCHES,
        *_BOOLEAN_FLAGS,
        *_OPTIONAL_VALUE_FLAGS,
        *_VALUE_FLAGS,
        *_FLAG_ALIASES.keys(),
        *_FLAG_ALIASES.values(),
    }
)


@dataclass(frozen=True)
class ClaudeArgsSpec(BaseArgsSpec):
    """Normalized representation of Claude CLI arguments.

    Inherits common fields and methods from BaseArgsSpec.
    Adds Claude-specific fields: is_background, is_fork.
    """

    is_background: bool = False
    is_fork: bool = False

    def update(
        self,
        *,
        background: bool | None = None,
        prompt: str | None = None,
    ) -> "ClaudeArgsSpec":
        """Return new spec with requested updates applied."""
        tokens = list(self.clean_tokens)
        positional_indexes = self.positional_indexes

        if background is not None:
            tokens = _toggle_background(tokens, positional_indexes, background)
            # Recalculate positional indexes after toggle
            temp_spec = _parse_tokens(tokens, self.source)
            positional_indexes = temp_spec.positional_indexes

        if prompt is not None:
            if prompt == "":
                # Empty string = delete positional arg
                tokens = _remove_positional(tokens, positional_indexes)
            else:
                tokens = _set_positional(tokens, prompt, positional_indexes)

        return _parse_tokens(tokens, self.source)

    def get_flag_value(self, flag_name: str) -> str | None:
        """Get value of any flag by searching clean_tokens.

        Searches for both space-separated (--flag value) and equals-form (--flag=value).
        Handles registered aliases (e.g., --allowed-tools and --allowedtools return same value).
        Returns the LAST occurrence to match Claude CLI "last wins" semantics.
        Returns None if flag not found.

        Examples:
            spec.get_flag_value('--output-format')
            spec.get_flag_value('--model')
            spec.get_flag_value('-r')  # Short form for --resume
        """
        flag_lower = flag_name.lower()

        # Build list of possible flag names (original + aliases)
        possible_flags = {flag_lower}

        # Add canonical form if this is an alias
        if flag_lower in _FLAG_ALIASES:
            canonical = _FLAG_ALIASES[flag_lower]
            possible_flags.add(canonical.lower())

        # Add all aliases that map to same canonical
        for alias, canonical in _FLAG_ALIASES.items():
            if canonical.lower() == flag_lower or alias.lower() == flag_lower:
                possible_flags.add(alias.lower())
                possible_flags.add(canonical.lower())

        # Include optional flag aliases (e.g., -r <-> --resume)
        if flag_lower in _OPTIONAL_ALIAS_LOOKUP:
            possible_flags.update(_OPTIONAL_ALIAS_LOOKUP[flag_lower])

        # Scan clean_tokens once in order, checking both --flag=value and --flag value forms
        # This preserves chronological order for "last wins" semantics
        last_value = None
        i = 0
        while i < len(self.clean_tokens):
            token = self.clean_tokens[i]
            token_lower = token.lower()

            # Check --flag=value form first
            for possible_flag in possible_flags:
                if token_lower.startswith(possible_flag + "="):
                    last_value = token[len(possible_flag) + 1 :]
                    break
            else:
                # Check --flag value form (space-separated)
                if token_lower in possible_flags:
                    # Found flag, check if next token is the value
                    if i + 1 < len(self.clean_tokens):
                        next_token = self.clean_tokens[i + 1]
                        # Ensure next token isn't another flag
                        if not _looks_like_new_flag(next_token.lower()):
                            last_value = next_token

            i += 1

        return last_value


def resolve_claude_args(
    cli_args: Sequence[str] | None,
    env_value: str | None,
) -> ClaudeArgsSpec:
    """Resolve Claude args from CLI (highest precedence) or env string."""
    if cli_args:
        return _parse_tokens(cli_args, "cli")

    if env_value is not None:
        try:
            tokens = _split_env(env_value)
        except ValueError as err:
            return _parse_tokens([], "env", initial_errors=[f"invalid Claude args: {err}"])
        return _parse_tokens(tokens, "env")

    return _parse_tokens([], "none")


def merge_claude_args(env_spec: ClaudeArgsSpec, cli_spec: ClaudeArgsSpec) -> ClaudeArgsSpec:
    """Merge env and CLI specs with smart precedence rules.

    Rules:
    1. If CLI has positional args, they REPLACE all env positionals
       - Empty string positional ("") explicitly deletes env positionals
       - No CLI positional means inherit env positionals
    2. CLI flags override env flags (per-flag precedence)
    3. Duplicate boolean flags are deduped
    4. System prompts treated like all other value flags (CLI overrides env)

    Args:
        env_spec: Parsed spec from HCOM_CLAUDE_ARGS env
        cli_spec: Parsed spec from CLI forwarded args

    Returns:
        Merged ClaudeArgsSpec with CLI taking precedence
    """
    # Handle positionals: CLI replaces env (if present), else inherit env
    final_positionals: TokenList
    if cli_spec.positional_tokens:
        # Check for empty string deletion marker
        if cli_spec.positional_tokens == ("",):
            final_positionals = []
        else:
            final_positionals = list(cli_spec.positional_tokens)
    else:
        # No CLI positional → inherit env positional
        final_positionals = list(env_spec.positional_tokens)

    # Extract flag names from CLI to know what to override
    cli_flag_names: set[str] = _extract_flag_names_from_tokens(cli_spec.clean_tokens)

    # Use index-based filtering to avoid false collisions with positional values
    env_positional_indexes: set[int] = set(env_spec.positional_indexes)
    cli_positional_indexes: set[int] = set(cli_spec.positional_indexes)

    # Build merged tokens: env flags (not overridden, not positionals) + CLI flags (not positionals)
    merged_tokens: TokenList = []
    skip_next: bool = False

    for i, token in enumerate(env_spec.clean_tokens):
        if skip_next:
            skip_next = False
            continue

        # Skip positionals by index (will be added explicitly later)
        if i in env_positional_indexes:
            continue

        # Check if this is a flag that CLI overrides
        flag_name = _extract_flag_name_from_token(token)
        if flag_name and flag_name in cli_flag_names:
            # CLI overrides this flag, skip env version
            # Check if next token is the value (space-separated syntax)
            if "=" not in token and i + 1 < len(env_spec.clean_tokens):
                next_token = env_spec.clean_tokens[i + 1]
                # Only skip next if it's not a known flag (it's the value)
                if not _looks_like_new_flag(next_token.lower()):
                    skip_next = True
            continue

        merged_tokens.append(token)

    # Append all CLI tokens (excluding positionals by index)
    for i, token in enumerate(cli_spec.clean_tokens):
        if i not in cli_positional_indexes:
            merged_tokens.append(token)

    # Deduplicate boolean flags
    merged_tokens = _deduplicate_boolean_flags(merged_tokens)

    # Rebuild spec from merged tokens
    # Need to combine tokens and positionals properly
    combined_tokens = list(merged_tokens)

    # Insert positionals at correct position
    # Find where positionals should go (after flags, before --)
    insert_idx = len(combined_tokens)
    try:
        dash_idx = combined_tokens.index("--")
        insert_idx = dash_idx
    except ValueError:
        pass

    # Insert positionals before -- (or at end)
    for pos in reversed(final_positionals):
        combined_tokens.insert(insert_idx, pos)

    # Re-parse to get proper ClaudeArgsSpec with all fields populated
    return _parse_tokens(combined_tokens, "cli")


def _deduplicate_boolean_flags(tokens: Sequence[str]) -> TokenList:
    """Remove duplicate boolean flags, keeping first occurrence.

    Only deduplicates known boolean flags like --verbose, -p, etc.
    Unknown flags and value flags are left as-is (Claude CLI handles them).
    """
    # Combine boolean flags and background switches for deduplication
    all_boolean_flags = _BOOLEAN_FLAGS | _BACKGROUND_SWITCHES | _FORK_SWITCHES
    return _deduplicate_boolean_flags_base(tokens, all_boolean_flags)


def add_background_defaults(spec: ClaudeArgsSpec) -> ClaudeArgsSpec:
    """Add HCOM-specific background mode defaults if missing.

    When background mode is detected (-p/--print), adds:
    - --output-format stream-json (if not already set)
    - --verbose (if not already set)

    Returns unchanged spec if not in background mode or flags already present.
    """
    if not spec.is_background:
        return spec

    tokens: TokenList = list(spec.clean_tokens)
    modified: bool = False

    # Find -- separator index if present
    insert_idx: int
    try:
        dash_idx: int = tokens.index("--")
        insert_idx = dash_idx
    except ValueError:
        insert_idx = len(tokens)

    # Add --output-format stream-json if missing (insert before --)
    if not spec.has_flag(["--output-format"], ("--output-format=",)):
        tokens.insert(insert_idx, "stream-json")
        tokens.insert(insert_idx, "--output-format")
        modified = True
        insert_idx += 2  # Adjust insert position

    # Add --verbose if missing (insert before --)
    if not spec.has_flag(["--verbose"]):
        tokens.insert(insert_idx, "--verbose")
        modified = True

    if not modified:
        return spec

    # Re-parse to get updated spec
    return _parse_tokens(tokens, spec.source)


def validate_conflicts(spec: ClaudeArgsSpec) -> list[str]:
    """Check for conflicting flag combinations.

    Returns list of warning messages for:
    - Multiple system prompts (informational, not an error)
    - Other known conflicts

    Empty list means no conflicts detected.
    """
    warnings: list[str] = []

    # Count system flags in clean_tokens
    system_flags: list[str] = []
    i: int = 0
    while i < len(spec.clean_tokens):
        token_lower: str = spec.clean_tokens[i].lower()
        if token_lower in ("--system-prompt", "--append-system-prompt"):
            system_flags.append(token_lower)
        elif token_lower.startswith(("--system-prompt=", "--append-system-prompt=")):
            system_flags.append(token_lower.split("=")[0])
        i += 1

    # Check for unusual system prompt combinations
    if len(system_flags) > 1:
        # Standard pattern: one --system-prompt and one --append-system-prompt (no warning)
        is_standard_pattern: bool = (
            len(system_flags) == 2 and "--system-prompt" in system_flags and "--append-system-prompt" in system_flags
        )
        if not is_standard_pattern:
            warnings.append(f"Multiple system prompts: {', '.join(system_flags)}. All included in order.")

    # Could add more conflict checks here:
    # - --print with interactive-only flags
    # - Conflicting permission modes
    # etc.

    return warnings


def _parse_tokens(
    tokens: Sequence[str],
    source: SourceType,
    initial_errors: Sequence[str] | None = None,
) -> ClaudeArgsSpec:
    """Parse CLI tokens into structured ClaudeArgsSpec.

    State machine that processes tokens sequentially, tracking:
    - pending_canonical: Known flag awaiting its value (e.g., --model expects next token)
    - pending_generic_flag: Unknown value-taking flag awaiting its value
    - after_double_dash: Whether we've passed '--' (all subsequent tokens are positional)

    Validates unknown options and suggests close matches. Detects --background for
    async launch mode. Preserves original token order in clean_tokens while extracting
    flag_values dict for known canonical flags.
    """
    errors: list[str] = list(initial_errors or [])
    clean: TokenList = []
    positional: TokenList = []
    positional_indexes: list[int] = []
    flag_values: FlagValuesDict = {}

    pending_canonical: CanonicalFlag | None = None
    pending_canonical_token: str | None = None
    pending_generic_flag: str | None = None
    after_double_dash: bool = False

    is_background: bool = False
    is_fork: bool = False

    i: int = 0
    raw_tokens: TokenTuple = tuple(tokens)

    while i < len(tokens):
        token = tokens[i]
        token_lower = token.lower()
        advance = True

        if pending_canonical:
            if _looks_like_new_flag(token_lower):
                display = pending_canonical_token or pending_canonical
                errors.append(f"{display} requires a value before '{token}'")
                pending_canonical = None
                pending_canonical_token = None
                advance = False
            else:
                idx = len(clean)
                clean.append(token)
                if after_double_dash:
                    positional.append(token)
                    positional_indexes.append(idx)
                flag_values[pending_canonical] = token
                pending_canonical = None
                pending_canonical_token = None
            if advance:
                i += 1
            continue

        if pending_generic_flag:
            if _looks_like_new_flag(token_lower):
                errors.append(f"{pending_generic_flag} requires a value before '{token}'")
                pending_generic_flag = None
                advance = False
            else:
                idx = len(clean)
                clean.append(token)
                if after_double_dash:
                    positional.append(token)
                    positional_indexes.append(idx)
                pending_generic_flag = None
            if advance:
                i += 1
            continue

        if after_double_dash:
            idx = len(clean)
            clean.append(token)
            positional.append(token)
            positional_indexes.append(idx)
            i += 1
            continue

        if token_lower == "--":
            clean.append(token)
            after_double_dash = True
            i += 1
            continue

        if token_lower in _BACKGROUND_SWITCHES:
            is_background = True
            clean.append(token)
            i += 1
            continue

        if token_lower in _FORK_SWITCHES:
            is_fork = True
            clean.append(token)
            i += 1
            continue

        if token_lower in _BOOLEAN_FLAGS:
            clean.append(token)
            i += 1
            continue

        canonical_assignment = _extract_canonical_prefixed(token, token_lower)
        if canonical_assignment:
            canonical_flag, value = canonical_assignment
            clean.append(token)
            flag_values[canonical_flag] = value
            i += 1
            continue

        if any(token_lower.startswith(prefix) for prefix in _VALUE_FLAG_PREFIXES):
            clean.append(token)
            i += 1
            continue

        if token_lower in _FLAG_ALIASES:
            pending_canonical = _FLAG_ALIASES[token_lower]
            pending_canonical_token = token
            clean.append(token)
            i += 1
            continue

        # Handle optional value flags (--resume, --debug, etc.)
        optional_assignment = None
        for prefix in _OPTIONAL_VALUE_FLAG_PREFIXES:
            if token_lower.startswith(prefix):
                # --resume=value or --debug=filter form
                optional_assignment = token
                break

        if optional_assignment:
            clean.append(token)
            i += 1
            continue

        if token_lower in _OPTIONAL_VALUE_FLAGS:
            # Peek ahead - only consume value if it's not a flag
            if i + 1 < len(tokens):
                next_token = tokens[i + 1]
                next_lower = next_token.lower()
                if not _looks_like_new_flag(next_lower):
                    # Has a value, treat as value flag
                    pending_generic_flag = token
                    clean.append(token)
                    i += 1
                    continue
            # No value or next is a flag - just add the flag alone
            clean.append(token)
            i += 1
            continue

        if token_lower in _VALUE_FLAGS:
            pending_generic_flag = token
            clean.append(token)
            i += 1
            continue

        # Unknown option detection (friendly early error vs launching Claude and failing later).
        # Convention: if user wants prompt text that starts with '-', they can use '--' to separate.
        if token_lower.startswith("--") or (
            token_lower.startswith("-") and len(token_lower) == 2 and token_lower[1].isalpha()
        ):
            if not _looks_like_new_flag(token_lower):
                base = token.split("=", 1)[0]
                suggested = difflib.get_close_matches(base, _KNOWN_CLAUDE_FLAGS, n=1, cutoff=0.6)
                if suggested:
                    errors.append(
                        f"unknown option '{token}' (did you mean {suggested[0]}?). "
                        "If this was prompt text, pass '--' before it."
                    )
                else:
                    errors.append(f"unknown option '{token}'. If this was prompt text, pass '--' before it.")
                clean.append(token)
                i += 1
                continue

        idx = len(clean)
        clean.append(token)
        if not _looks_like_new_flag(token_lower):
            positional.append(token)
            positional_indexes.append(idx)
        i += 1

    if pending_canonical:
        display = pending_canonical_token or pending_canonical
        errors.append(f"{display} requires a value at end of arguments")
    if pending_generic_flag:
        errors.append(f"{pending_generic_flag} requires a value at end of arguments")

    return ClaudeArgsSpec(
        source=source,
        raw_tokens=raw_tokens,
        clean_tokens=tuple(clean),
        positional_tokens=tuple(positional),
        positional_indexes=tuple(positional_indexes),
        is_background=is_background,
        is_fork=is_fork,
        flag_values=dict(flag_values),
        errors=tuple(errors),
    )


def _split_env(env_value: str) -> TokenList:
    """Split shell-quoted environment variable into tokens."""
    return shlex.split(env_value)


def _extract_canonical_prefixed(token: str, token_lower: str) -> tuple[CanonicalFlag, str] | None:
    """Extract canonical flag and value from --flag=value syntax.

    Returns (canonical_flag, value) tuple if token matches a known prefix,
    or None if not a recognized prefixed flag.
    """
    for prefix, canonical in _CANONICAL_PREFIXES.items():
        if token_lower.startswith(prefix):
            return canonical, token[len(prefix) :]
    return None


def _looks_like_new_flag(token_lower: str) -> bool:
    """Check if token looks like a flag (not a value).

    Used to detect when a flag is missing its value (next token is another flag).
    Recognizes known flags explicitly, no catch-all hyphen check.

    NOTE: No catch-all token_lower.startswith("-") check here!
    That would reject valid values like "- check something" or "-1"
    Instead, we explicitly list known boolean flags.
    """
    return _looks_like_flag_base(
        token_lower,
        boolean_flags=_BOOLEAN_FLAGS,
        value_flags=_VALUE_FLAGS,
        value_flag_prefixes=_VALUE_FLAG_PREFIXES,
        optional_value_flags=_OPTIONAL_VALUE_FLAGS,
        flag_aliases=_FLAG_ALIASES,
        optional_value_flag_prefixes=_OPTIONAL_VALUE_FLAG_PREFIXES,
        canonical_prefixes=_CANONICAL_PREFIXES,
        extra_flags=_BACKGROUND_SWITCHES | _FORK_SWITCHES,
    )


def _toggle_background(tokens: Sequence[str], positional_indexes: tuple[int, ...], desired: bool) -> TokenList:
    """Toggle background flag, preserving positional arguments.

    Args:
        tokens: Token list to process
        positional_indexes: Indexes of positional arguments (not to be filtered)
        desired: True to enable background mode, False to disable

    Returns:
        Modified token list with background flag toggled
    """
    tokens_list: TokenList = list(tokens)

    # Only filter tokens that are NOT positionals
    filtered: TokenList = []
    for idx, token in enumerate(tokens_list):
        if idx in positional_indexes:
            # Keep positionals even if they look like flags
            filtered.append(token)
        elif token.lower() not in _BACKGROUND_SWITCHES:
            filtered.append(token)

    has_background: bool = len(filtered) != len(tokens_list)

    if desired:
        if has_background:
            return tokens_list
        return ["-p"] + filtered
    return filtered


