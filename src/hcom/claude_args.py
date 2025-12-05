#!/usr/bin/env python3
"""Claude CLI argument parsing and validation"""
from __future__ import annotations

import shlex
from dataclasses import dataclass
from typing import Iterable, Literal, Mapping, Sequence, Tuple


CanonicalFlag = Literal["--model", "--allowedTools", "--disallowedTools"]

# All flag keys stored in lowercase (aside from short-form switches) for comparisons;
# values use canonical casing when recorded.
_FLAG_ALIASES: Mapping[str, CanonicalFlag] = {
    "--model": "--model",
    "--allowedtools": "--allowedTools",
    "--allowed-tools": "--allowedTools",
    "--disallowedtools": "--disallowedTools",
    "--disallowed-tools": "--disallowedTools",
}

_CANONICAL_PREFIXES: Mapping[str, CanonicalFlag] = {
    "--model=": "--model",
    "--allowedtools=": "--allowedTools",
    "--allowed-tools=": "--allowedTools",
    "--disallowedtools=": "--disallowedTools",
    "--disallowed-tools=": "--disallowedTools",
}

# Background switches: NOT in _BOOLEAN_FLAGS to enable special handling.
# has_flag() detects them by scanning clean_tokens directly, so they remain
# discoverable via spec.has_flag(['-p']) or spec.has_flag(['--print']).
_BACKGROUND_SWITCHES = {"-p", "--print"}
_SYSTEM_FLAGS = {"--system-prompt", "--append-system-prompt"}
_BOOLEAN_FLAGS = {
    "--verbose",
    "--continue", "-c",
    "--dangerously-skip-permissions",
    "--include-partial-messages",
    "--allow-dangerously-skip-permissions",
    "--replay-user-messages",
    "--mcp-debug",
    "--fork-session",
    "--ide",
    "--strict-mcp-config",
    "-v", "--version",
    "-h", "--help",
}
_SYSTEM_PREFIXES: Mapping[str, str] = {
    "--system-prompt=": "--system-prompt",
    "--append-system-prompt=": "--append-system-prompt",
}

# Flags with optional values (lowercase).
_OPTIONAL_VALUE_FLAGS = {
    "--resume", "-r",
    "--debug", "-d",
}

_OPTIONAL_VALUE_FLAG_PREFIXES = {
    "--resume=", "-r=",
    "--debug=", "-d=",
}

_OPTIONAL_ALIAS_GROUPS = (
    frozenset({"--resume", "-r"}),
    frozenset({"--debug", "-d"}),
)
_OPTIONAL_ALIAS_LOOKUP: Mapping[str, set[str]] = {
    alias: set(group)
    for group in _OPTIONAL_ALIAS_GROUPS
    for alias in group
}

# Flags that require a following value (lowercase).
_VALUE_FLAGS = {
    "--add-dir",
    "--agent",
    "--agents",
    "--allowed-tools",
    "--allowedtools",
    "--betas",
    "--disallowedtools",
    "--disallowed-tools",
    "--fallback-model",
    "--input-format",
    "--json-schema",
    "--max-turns",
    "--mcp-config",
    "--model",
    "--output-format",
    "--permission-mode",
    "--permission-prompt-tool",
    "--plugin-dir",
    "--session-id",
    "--setting-sources",
    "--settings",
    "--system-prompt-file",
    "--tools",
}

_VALUE_FLAG_PREFIXES = {
    "--add-dir=",
    "--agent=",
    "--agents=",
    "--allowedtools=",
    "--allowed-tools=",
    "--betas=",
    "--disallowedtools=",
    "--disallowed-tools=",
    "--fallback-model=",
    "--input-format=",
    "--json-schema=",
    "--max-turns=",
    "--mcp-config=",
    "--model=",
    "--output-format=",
    "--permission-mode=",
    "--permission-prompt-tool=",
    "--plugin-dir=",
    "--session-id=",
    "--setting-sources=",
    "--settings=",
    "--system-prompt-file=",
    "--tools=",
}


@dataclass(frozen=True)
class ClaudeArgsSpec:
    """Normalized representation of Claude CLI arguments."""

    source: Literal["cli", "env", "none"]
    raw_tokens: Tuple[str, ...]
    clean_tokens: Tuple[str, ...]
    positional_tokens: Tuple[str, ...]
    positional_indexes: Tuple[int, ...]
    system_entries: Tuple[Tuple[str, str], ...]
    system_flag: str | None
    system_value: str | None
    user_system: str | None
    user_append: str | None
    is_background: bool
    flag_values: Mapping[CanonicalFlag, str]
    errors: Tuple[str, ...] = ()

    def has_flag(
        self,
        names: Iterable[str] | None = None,
        prefixes: Iterable[str] | None = None,
    ) -> bool:
        """Check for user-provided flags (only scans before -- separator)."""
        name_set = {n.lower() for n in (names or ())}
        prefix_tuple = tuple(p.lower() for p in (prefixes or ()))

        # Only scan tokens before --
        try:
            dash_idx = self.clean_tokens.index('--')
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

    def rebuild_tokens(self, include_system: bool = True) -> list[str]:
        """Return token list suitable for invoking Claude."""
        tokens = list(self.clean_tokens)
        if include_system and self.system_entries:
            for flag, value in self.system_entries:
                tokens.extend([flag, value])
        return tokens

    def to_env_string(self) -> str:
        """Render tokens into a shell-safe env string."""
        return shlex.join(self.rebuild_tokens())

    def update(
        self,
        *,
        background: bool | None = None,
        system_flag: str | None = None,
        system_value: str | None = None,
        prompt: str | None = None,
    ) -> "ClaudeArgsSpec":
        """Return new spec with requested updates applied."""
        tokens = list(self.clean_tokens)

        if background is not None:
            tokens = _toggle_background(tokens, self.positional_indexes, background)

        if prompt is not None:
            if prompt == "":
                # Empty string = delete positional arg
                tokens = _remove_positional(tokens)
            else:
                tokens = _set_prompt(tokens, prompt)

        updated_entries = list(self.system_entries)

        # Interpret explicit updates
        if system_flag is not None or system_value is not None:
            if system_value == "":
                updated_entries.clear()
            else:
                current_flag = updated_entries[-1][0] if updated_entries else None
                current_value = updated_entries[-1][1] if updated_entries else None

                if system_flag is not None:
                    if system_flag:
                        current_flag = system_flag
                    else:
                        current_flag = None

                if system_value is not None:
                    current_value = system_value

                if current_flag is None or current_value is None:
                    updated_entries.clear()
                else:
                    if updated_entries:
                        updated_entries[-1] = (current_flag, current_value)
                    else:
                        updated_entries.append((current_flag, current_value))

        combined = list(tokens)
        for flag, value in updated_entries:
            combined.extend([flag, value])

        return _parse_tokens(combined, self.source)

    def has_errors(self) -> bool:
        return bool(self.errors)

    def get_flag_value(self, flag_name: str) -> str | None:
        """Get value of any flag by searching clean_tokens.

        Searches for both space-separated (--flag value) and equals-form (--flag=value).
        Handles registered aliases (e.g., --allowed-tools and --allowedtools return same value).
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

        # Check for --flag=value form in clean_tokens
        for token in self.clean_tokens:
            token_lower = token.lower()
            for possible_flag in possible_flags:
                if token_lower.startswith(possible_flag + '='):
                    return token[len(possible_flag) + 1:]

        # Check for --flag value form (space-separated)
        i = 0
        while i < len(self.clean_tokens):
            token_lower = self.clean_tokens[i].lower()
            if token_lower in possible_flags:
                # Found flag, check if next token is the value
                if i + 1 < len(self.clean_tokens):
                    next_token = self.clean_tokens[i + 1]
                    # Ensure next token isn't another flag
                    if not _looks_like_new_flag(next_token.lower()):
                        return next_token
                return None  # Flag present but no value
            i += 1

        return None


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
    4. System prompts handled separately via system_entries

    Args:
        env_spec: Parsed spec from HCOM_CLAUDE_ARGS env
        cli_spec: Parsed spec from CLI forwarded args

    Returns:
        Merged ClaudeArgsSpec with CLI taking precedence
    """
    # Handle positionals: CLI replaces env (if present), else inherit env
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
    cli_flag_names = _extract_flag_names_from_tokens(cli_spec.clean_tokens)

    # Filter out positionals from env and CLI clean_tokens to avoid duplication
    env_positional_set = set(env_spec.positional_tokens)
    cli_positional_set = set(cli_spec.positional_tokens)

    # Build merged tokens: env flags (not overridden, not positionals) + CLI flags (not positionals)
    merged_tokens = []
    skip_next = False

    for i, token in enumerate(env_spec.clean_tokens):
        if skip_next:
            skip_next = False
            continue

        # Skip positionals (will be added explicitly later)
        if token in env_positional_set:
            continue

        # Check if this is a flag that CLI overrides
        flag_name = _extract_flag_name_from_token(token)
        if flag_name and flag_name in cli_flag_names:
            # CLI overrides this flag, skip env version
            # Check if next token is the value (space-separated syntax)
            if '=' not in token and i + 1 < len(env_spec.clean_tokens):
                next_token = env_spec.clean_tokens[i + 1]
                # Only skip next if it's not a known flag (it's the value)
                if not _looks_like_new_flag(next_token.lower()):
                    skip_next = True
            continue

        merged_tokens.append(token)

    # Append all CLI tokens (excluding positionals)
    for token in cli_spec.clean_tokens:
        if token not in cli_positional_set:
            merged_tokens.append(token)

    # Deduplicate boolean flags
    merged_tokens = _deduplicate_boolean_flags(merged_tokens)

    # Handle system prompts: CLI wins if present, else env
    if cli_spec.system_entries:
        system_entries = cli_spec.system_entries
    else:
        system_entries = env_spec.system_entries

    # Rebuild spec from merged tokens
    # Need to combine tokens and positionals properly
    combined_tokens = list(merged_tokens)

    # Insert positionals at correct position
    # Find where positionals should go (after flags, before --)
    insert_idx = len(combined_tokens)
    try:
        dash_idx = combined_tokens.index('--')
        insert_idx = dash_idx
    except ValueError:
        pass

    # Insert positionals before -- (or at end)
    for pos in reversed(final_positionals):
        combined_tokens.insert(insert_idx, pos)

    # Add system prompts back
    for flag, value in system_entries:
        combined_tokens.extend([flag, value])

    # Re-parse to get proper ClaudeArgsSpec with all fields populated
    return _parse_tokens(combined_tokens, "cli")


def _extract_flag_names_from_tokens(tokens: Sequence[str]) -> set[str]:
    """Extract normalized flag names from token list.

    Returns set of lowercase flag names (without values).
    Examples: '--model' → '--model', '--model=opus' → '--model'
    """
    flag_names = set()
    for token in tokens:
        flag_name = _extract_flag_name_from_token(token)
        if flag_name:
            flag_names.add(flag_name)
    return flag_names


def _extract_flag_name_from_token(token: str) -> str | None:
    """Extract flag name from a token.

    Examples:
        '--model' → '--model'
        '--model=opus' → '--model'
        '-p' → '-p'
        'value' → None
    """
    token_lower = token.lower()

    # Check if starts with - or --
    if not token_lower.startswith('-'):
        return None

    # Extract name (before = if present)
    if '=' in token_lower:
        return token_lower.split('=')[0]

    return token_lower


def _deduplicate_boolean_flags(tokens: Sequence[str]) -> list[str]:
    """Remove duplicate boolean flags, keeping first occurrence.

    Only deduplicates known boolean flags like --verbose, -p, etc.
    Unknown flags and value flags are left as-is (Claude CLI handles them).
    """
    seen_flags = set()
    result = []

    for token in tokens:
        token_lower = token.lower()

        # Check if this is a known boolean flag
        if token_lower in _BOOLEAN_FLAGS or token_lower in _BACKGROUND_SWITCHES:
            if token_lower in seen_flags:
                continue  # Skip duplicate
            seen_flags.add(token_lower)

        result.append(token)

    return result


def add_background_defaults(spec: ClaudeArgsSpec) -> ClaudeArgsSpec:
    """Add HCOM-specific background mode defaults if missing.

    When background mode is detected (-p/--print), adds:
    - --output-format stream-json (if not already set)
    - --verbose (if not already set)

    Returns unchanged spec if not in background mode or flags already present.
    """
    if not spec.is_background:
        return spec

    tokens = list(spec.clean_tokens)
    modified = False

    # Find -- separator index if present
    try:
        dash_idx = tokens.index('--')
        insert_idx = dash_idx
    except ValueError:
        insert_idx = len(tokens)

    # Add --output-format stream-json if missing (insert before --)
    if not spec.has_flag(['--output-format'], ('--output-format=',)):
        tokens.insert(insert_idx, 'stream-json')
        tokens.insert(insert_idx, '--output-format')
        modified = True
        insert_idx += 2  # Adjust insert position

    # Add --verbose if missing (insert before --)
    if not spec.has_flag(['--verbose']):
        tokens.insert(insert_idx, '--verbose')
        modified = True

    if not modified:
        return spec

    # Re-parse to get updated spec with system entries preserved
    combined = tokens[:]
    for flag, value in spec.system_entries:
        combined.extend([flag, value])

    return _parse_tokens(combined, spec.source)


def validate_conflicts(spec: ClaudeArgsSpec) -> list[str]:
    """Check for conflicting flag combinations.

    Returns list of warning messages for:
    - Multiple system prompts (informational, not an error)
    - Other known conflicts

    Empty list means no conflicts detected.
    """
    warnings = []

    # Check for unusual system prompt combinations (not the standard --system-prompt + --append-system-prompt)
    if len(spec.system_entries) > 1:
        flags = [f for f, _ in spec.system_entries]
        # Standard pattern: one --system-prompt and one --append-system-prompt (no warning)
        is_standard_pattern = (
            len(flags) == 2 and
            '--system-prompt' in flags and
            '--append-system-prompt' in flags
        )
        if not is_standard_pattern:
            warnings.append(
                f"Multiple system prompts detected: {', '.join(flags)}. "
                f"All will be included in order."
            )

    # Could add more conflict checks here:
    # - --print with interactive-only flags
    # - Conflicting permission modes
    # etc.

    return warnings


def _parse_tokens(
    tokens: Sequence[str],
    source: Literal["cli", "env", "none"],
    initial_errors: Sequence[str] | None = None,
) -> ClaudeArgsSpec:
    errors = list(initial_errors or [])
    clean: list[str] = []
    positional: list[str] = []
    positional_indexes: list[int] = []
    flag_values: dict[CanonicalFlag, str] = {}
    system_entries: list[Tuple[str, str]] = []

    pending_system: str | None = None
    pending_canonical: CanonicalFlag | None = None
    pending_canonical_token: str | None = None
    pending_generic_flag: str | None = None
    after_double_dash = False

    system_flag: str | None = None
    system_value: str | None = None
    user_system: str | None = None
    user_append: str | None = None
    is_background = False

    i = 0
    raw_tokens = tuple(tokens)

    while i < len(tokens):
        token = tokens[i]
        token_lower = token.lower()
        advance = True

        if pending_system:
            if _looks_like_new_flag(token_lower):
                errors.append(f"{pending_system} requires a value before '{token}'")
                pending_system = None
                advance = False
            else:
                system_entries.append((pending_system, token))
                system_flag = pending_system
                system_value = token
                if pending_system == "--system-prompt":
                    user_system = token
                else:
                    user_append = token
                pending_system = None
            if advance:
                i += 1
            continue

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

        if token_lower in _BOOLEAN_FLAGS:
            clean.append(token)
            i += 1
            continue

        system_assignment = _extract_system_assignment(token, token_lower)
        if system_assignment:
            assigned_flag, value = system_assignment
            system_entries.append((assigned_flag, value))
            system_flag = assigned_flag
            system_value = value
            if assigned_flag == "--system-prompt":
                user_system = value
            else:
                user_append = value
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

        # System prompts: intentionally excluded from clean_tokens and tracked
        # via system_entries for special merge/update behavior. DO NOT add these
        # to _VALUE_FLAGS - would break the special handling path.
        if token_lower in _SYSTEM_FLAGS:
            pending_system = "--system-prompt" if token_lower == "--system-prompt" else "--append-system-prompt"
            i += 1
            continue

        idx = len(clean)
        clean.append(token)
        if not _looks_like_new_flag(token_lower):
            positional.append(token)
            positional_indexes.append(idx)
        i += 1

    if pending_system:
        errors.append(f"{pending_system} requires a value at end of arguments")
    if pending_canonical:
        display = pending_canonical_token or pending_canonical
        errors.append(f"{display} requires a value at end of arguments")
    if pending_generic_flag:
        errors.append(f"{pending_generic_flag} requires a value at end of arguments")

    last_flag = system_entries[-1][0] if system_entries else None
    last_value = system_entries[-1][1] if system_entries else None

    return ClaudeArgsSpec(
        source=source,
        raw_tokens=raw_tokens,
        clean_tokens=tuple(clean),
        positional_tokens=tuple(positional),
        positional_indexes=tuple(positional_indexes),
        system_entries=tuple(system_entries),
        system_flag=last_flag,
        system_value=last_value,
        user_system=user_system,
        user_append=user_append,
        is_background=is_background,
        flag_values=dict(flag_values),
        errors=tuple(errors),
    )


def _split_env(env_value: str) -> list[str]:
    return shlex.split(env_value)


def _extract_system_assignment(token: str, token_lower: str) -> tuple[str, str] | None:
    for prefix, canonical in _SYSTEM_PREFIXES.items():
        if token_lower.startswith(prefix):
            value = token[len(prefix):]
            return canonical, value
    return None


def _extract_canonical_prefixed(token: str, token_lower: str) -> tuple[CanonicalFlag, str] | None:
    for prefix, canonical in _CANONICAL_PREFIXES.items():
        if token_lower.startswith(prefix):
            return canonical, token[len(prefix):]
    return None


def _looks_like_new_flag(token_lower: str) -> bool:
    """Check if token looks like a flag (not a value).

    Used to detect when a flag is missing its value (next token is another flag).
    Recognizes known flags explicitly, no catch-all hyphen check.
    """
    if token_lower in _BACKGROUND_SWITCHES:
        return True
    if token_lower in _SYSTEM_FLAGS:
        return True
    if token_lower in _BOOLEAN_FLAGS:
        return True
    if token_lower in _FLAG_ALIASES:
        return True
    if token_lower in _OPTIONAL_VALUE_FLAGS:
        return True
    if token_lower in _VALUE_FLAGS:
        return True
    if token_lower == "--":
        return True
    if any(token_lower.startswith(prefix) for prefix in _OPTIONAL_VALUE_FLAG_PREFIXES):
        return True
    if any(token_lower.startswith(prefix) for prefix in _VALUE_FLAG_PREFIXES):
        return True
    if any(token_lower.startswith(prefix) for prefix in _SYSTEM_PREFIXES):
        return True
    if any(token_lower.startswith(prefix) for prefix in _CANONICAL_PREFIXES):
        return True
    # NOTE: No catch-all token_lower.startswith("-") check here!
    # That would reject valid values like "- check something" or "-1"
    # Instead, we explicitly list known boolean flags above
    return False


def _toggle_background(tokens: Sequence[str], positional_indexes: Tuple[int, ...], desired: bool) -> list[str]:
    """Toggle background flag, preserving positional arguments.

    Args:
        tokens: Token list to process
        positional_indexes: Indexes of positional arguments (not to be filtered)
        desired: True to enable background mode, False to disable

    Returns:
        Modified token list with background flag toggled
    """
    tokens_list = list(tokens)

    # Only filter tokens that are NOT positionals
    filtered = []
    for idx, token in enumerate(tokens_list):
        if idx in positional_indexes:
            # Keep positionals even if they look like flags
            filtered.append(token)
        elif token.lower() not in _BACKGROUND_SWITCHES:
            filtered.append(token)

    has_background = len(filtered) != len(tokens_list)

    if desired:
        if has_background:
            return tokens_list
        return ["-p"] + filtered
    return filtered


def _set_prompt(tokens: Sequence[str], value: str) -> list[str]:
    tokens_list = list(tokens)
    index = _find_first_positional_index(tokens_list)
    if index is None:
        tokens_list.append(value)
    else:
        tokens_list[index] = value
    return tokens_list


def _remove_positional(tokens: Sequence[str]) -> list[str]:
    """Remove first positional argument from tokens"""
    tokens_list = list(tokens)
    index = _find_first_positional_index(tokens_list)
    if index is not None:
        tokens_list.pop(index)
    return tokens_list


def _find_first_positional_index(tokens: Sequence[str]) -> int | None:
    pending_system = False
    pending_canonical = False
    pending_generic = False
    after_double_dash = False

    for idx, token in enumerate(tokens):
        token_lower = token.lower()

        if after_double_dash:
            return idx
        if token_lower == "--":
            after_double_dash = True
            continue
        if pending_system:
            pending_system = False
            continue
        if pending_canonical:
            pending_canonical = False
            continue
        if pending_generic:
            pending_generic = False
            continue
        if token_lower in _BACKGROUND_SWITCHES:
            continue
        if token_lower in _BOOLEAN_FLAGS:
            continue
        if _extract_system_assignment(token, token_lower):
            continue
        if token_lower in _SYSTEM_FLAGS:
            pending_system = True
            continue
        if _extract_canonical_prefixed(token, token_lower):
            continue
        if any(token_lower.startswith(prefix) for prefix in _OPTIONAL_VALUE_FLAG_PREFIXES):
            continue
        if any(token_lower.startswith(prefix) for prefix in _VALUE_FLAG_PREFIXES):
            continue
        if token_lower in _FLAG_ALIASES:
            pending_canonical = True
            continue
        if token_lower in _OPTIONAL_VALUE_FLAGS:
            pending_generic = True
            continue
        if token_lower in _VALUE_FLAGS:
            pending_generic = True
            continue
        if _looks_like_new_flag(token_lower):
            continue
        return idx
    return None
