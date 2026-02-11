#!/usr/bin/env python3
"""Gemini CLI argument parsing and validation.

Parses Gemini CLI arguments with support for:
- Subcommands (mcp, extensions, hooks)
- Flag aliases (-m/--model, -p/--prompt, etc.)
- Boolean flags (--yolo, --debug, etc.)
- Value flags with space or equals syntax
- Optional value flags (--resume with optional session id)
- Repeatable flags (--extensions can appear multiple times)

Key Semantic Flags:
    is_headless: True when prompt provided via positional or -p/--prompt.
        hcom rejects headless Gemini - interactive mode with -i flag works.
    is_json: True when --output-format is json or stream-json.
    is_yolo: True when --yolo or --approval-mode=yolo (auto-approve all).

Usage:
    >>> spec = resolve_gemini_args(['--model', 'gemini-2.0'], None)
    >>> spec.get_flag_value('--model')
    'gemini-2.0'
"""

from __future__ import annotations

import difflib
import re
import shlex
from dataclasses import dataclass
from typing import Final, Literal, Mapping, Sequence, TypeAlias

from ..args_common import (
    BaseArgsSpec,
    SourceType,
    TokenList,
    TokenTuple,
    FlagValuesDict,
    extract_flag_names_from_tokens as _extract_flag_names_from_tokens,
    extract_flag_name_from_token as _extract_flag_name_from_token,
    deduplicate_boolean_flags as _deduplicate_boolean_flags_base,
    toggle_flag as _toggle_flag,
    set_value_flag as _set_value_flag,
    remove_flag_with_value as _remove_flag_with_value,
    looks_like_flag as _looks_like_flag_base,
)

# Type aliases (Gemini-specific)
Subcommand: TypeAlias = Literal["mcp", "extensions", "hooks", "skills", None]
OutputFormat: TypeAlias = Literal["text", "json", "stream-json"]
ApprovalMode: TypeAlias = Literal["default", "auto_edit", "yolo", "plan"]

# ==================== Flag Classification ====================

# Subcommand aliases
_SUBCOMMAND_ALIASES: Final[Mapping[str, str]] = {
    "extension": "extensions",
    "hook": "hooks",
    "skill": "skills",
}

# All known subcommands (from gemini --help)
_SUBCOMMANDS: Final[frozenset[str]] = frozenset(
    {
        "mcp",
        "extensions",
        "extension",  # extension is alias
        "hooks",
        "hook",  # hook is alias
        "skills",
        "skill",  # skill is alias
    }
)

# Flag aliases: map short forms to canonical long forms
_FLAG_ALIASES: Final[Mapping[str, str]] = {
    "-d": "--debug",
    "-m": "--model",
    "-p": "--prompt",
    "-i": "--prompt-interactive",
    "-s": "--sandbox",
    "-y": "--yolo",
    "-e": "--extensions",
    "-l": "--list-extensions",
    "-r": "--resume",
    "-o": "--output-format",
    "-v": "--version",
    "-h": "--help",
}

# Boolean flags (no value required)
_BOOLEAN_FLAGS: Final[frozenset[str]] = frozenset(
    {
        # Global
        "-d",
        "--debug",
        "-s",
        "--sandbox",
        "-y",
        "--yolo",
        "-l",
        "--list-extensions",
        "--list-sessions",
        "--screen-reader",
        "-v",
        "--version",
        "-h",
        "--help",
        "--experimental-acp",
        "--raw-output",
        "--accept-raw-output-risk",
    }
)

# Flags that require a following value
_VALUE_FLAGS: Final[frozenset[str]] = frozenset(
    {
        # Global (short and long forms)
        "-m",
        "--model",
        "-p",
        "--prompt",
        "-i",
        "--prompt-interactive",
        "--approval-mode",
        "--allowed-mcp-server-names",
        "--allowed-tools",
        "-e",
        "--extensions",
        "--delete-session",
        "--include-directories",
        "-o",
        "--output-format",
    }
)
# Note: --resume/-r moved to _OPTIONAL_VALUE_FLAGS

# Flags with = syntax prefixes
_VALUE_FLAG_PREFIXES: Final[frozenset[str]] = frozenset(
    {
        "-m=",
        "--model=",
        "-p=",
        "--prompt=",
        "-i=",
        "--prompt-interactive=",
        "--approval-mode=",
        "--allowed-mcp-server-names=",
        "--allowed-tools=",
        "-e=",
        "--extensions=",
        "-r=",
        "--resume=",
        "--delete-session=",
        "--include-directories=",
        "-o=",
        "--output-format=",
    }
)

# Repeatable flags (can appear multiple times)
_REPEATABLE_FLAGS: Final[frozenset[str]] = frozenset(
    {
        "-e",
        "--extensions",
        "--include-directories",
        "--allowed-mcp-server-names",
        "--allowed-tools",
    }
)

# Optional value flags (can be used with or without a value)
# Per gemini docs: --resume [session_id] defaults to "latest" if omitted
_OPTIONAL_VALUE_FLAGS: Final[frozenset[str]] = frozenset(
    {
        "--resume",
        "-r",
    }
)

_KNOWN_GEMINI_FLAGS: Final[list[str]] = sorted(
    {
        *_BOOLEAN_FLAGS,
        *_OPTIONAL_VALUE_FLAGS,
        *_VALUE_FLAGS,
        *_FLAG_ALIASES.keys(),
        *_FLAG_ALIASES.values(),
    }
)


@dataclass(frozen=True)
class GeminiArgsSpec(BaseArgsSpec):
    """Normalized representation of Gemini CLI arguments.

    Inherits common fields and methods from BaseArgsSpec.
    Key fields for launch logic:
    - is_headless: True = non-interactive mode detected (prompt provided).
      Used to REJECT such launches - headless Gemini not supported in hcom.
    - is_json: True = JSON output format (--output-format json|stream-json)
    - is_yolo: True = auto-approve all tools (--yolo or --approval-mode=yolo)
    """

    subcommand: Subcommand = None
    is_headless: bool = False  # Has prompt (positional, -p, or stdin)
    is_json: bool = False  # --output-format json|stream-json
    is_yolo: bool = False  # --yolo or --approval-mode=yolo
    output_format: OutputFormat = "text"
    approval_mode: ApprovalMode = "default"

    def get_flag_value(self, flag_name: str) -> str | list[str] | None:
        """Get value of a flag.

        For repeatable flags (--extensions, etc.), returns list.
        For single-value flags, returns the LAST occurrence to match CLI "last wins" semantics.
        Handles aliases: -m and --model both return same value.
        Returns None if flag not found.
        """
        # Build set of possible flag names (original + aliases)
        possible_flags = {flag_name.lower()}

        # Add alias mappings
        flag_lower = flag_name.lower()
        if flag_lower in _FLAG_ALIASES:
            possible_flags.add(_FLAG_ALIASES[flag_lower])
        # Reverse lookup: if given --model, also check -m
        for short, long in _FLAG_ALIASES.items():
            if long == flag_lower:
                possible_flags.add(short)

        # Check pre-parsed flag_values (already has "last wins" from parsing)
        for pf in possible_flags:
            if pf in self.flag_values:
                return self.flag_values[pf]

        # Fallback: scan clean_tokens once in order, checking both forms
        # This preserves chronological order for "last wins" semantics
        is_optional_flag = any(pf in _OPTIONAL_VALUE_FLAGS for pf in possible_flags)
        last_value = None
        i = 0
        while i < len(self.clean_tokens):
            token = self.clean_tokens[i]
            token_lower = token.lower()

            # Check --flag=value form first
            for pf in possible_flags:
                if token_lower.startswith(pf + "="):
                    last_value = token[len(pf) + 1 :]
                    break
            else:
                # Check --flag value form (space-separated)
                # For optional value flags, only use values from flag_values
                if token_lower in possible_flags and not is_optional_flag:
                    if i + 1 < len(self.clean_tokens):
                        next_token = self.clean_tokens[i + 1]
                        if not _looks_like_flag(next_token.lower()):
                            last_value = next_token

            i += 1

        return last_value

    def rebuild_tokens(
        self,
        include_positionals: bool = True,
        *,
        include_subcommand: bool = True,
    ) -> TokenList:
        """Return token list suitable for invoking Gemini.

        Overrides base class to prepend subcommand if present.

        Args:
            include_positionals: Include positional arguments (inherited from base)
            include_subcommand: Include subcommand prefix (Gemini-specific)
        """
        tokens: TokenList = []
        if include_subcommand and self.subcommand:
            tokens.append(self.subcommand)
        tokens.extend(super().rebuild_tokens(include_positionals))
        return tokens

    def update(
        self,
        *,
        json_output: bool | None = None,
        stream_json: bool | None = None,
        prompt: str | None = None,
        subcommand: Subcommand | str = None,
        yolo: bool | None = None,
        approval_mode: ApprovalMode | None = None,
        include_directories: list[str] | None = None,
    ) -> "GeminiArgsSpec":
        """Return new spec with requested updates applied.

        Args:
            json_output: Set --output-format json
            stream_json: Set --output-format stream-json
            prompt: Set positional prompt argument
            subcommand: Set subcommand (mcp/extensions/hooks)
            yolo: Set --yolo flag
            approval_mode: Set --approval-mode value
            include_directories: Set --include-directories values
        """
        tokens = list(self.clean_tokens)
        new_subcommand: str | None = self.subcommand

        if subcommand is not None:
            new_subcommand = subcommand if subcommand else None

        if yolo is not None:
            tokens = _toggle_flag(tokens, "--yolo", yolo)

        if approval_mode is not None:
            tokens = _set_value_flag(tokens, "--approval-mode", approval_mode)

        if json_output is True:
            tokens = _set_value_flag(tokens, "--output-format", "json")
        elif stream_json is True:
            tokens = _set_value_flag(tokens, "--output-format", "stream-json")

        if prompt is not None:
            # Use -i (interactive with initial prompt) instead of positional (headless)
            if prompt == "":
                tokens = _remove_flag_with_value(tokens, "-i")
                tokens = _remove_flag_with_value(tokens, "--prompt-interactive")
            else:
                tokens = _set_value_flag(tokens, "-i", prompt)

        if include_directories is not None:
            # Remove existing --include-directories
            tokens = _remove_flag_with_value(tokens, "--include-directories")
            for dir_path in include_directories:
                tokens.extend(["--include-directories", dir_path])

        # Rebuild with subcommand
        combined: list[str] = []
        if new_subcommand:
            combined.append(new_subcommand)
        combined.extend(tokens)

        return _parse_tokens(combined, self.source)


def resolve_gemini_args(
    cli_args: Sequence[str] | None,
    env_value: str | None,
) -> GeminiArgsSpec:
    """Resolve Gemini args from CLI (highest precedence) or env string."""
    if cli_args:
        return _parse_tokens(cli_args, "cli")

    if env_value is not None:
        try:
            tokens = shlex.split(env_value)
        except ValueError as err:
            return _parse_tokens([], "env", initial_errors=[f"invalid Gemini args: {err}"])
        return _parse_tokens(tokens, "env")

    return _parse_tokens([], "none")


def merge_gemini_args(env_spec: GeminiArgsSpec, cli_spec: GeminiArgsSpec) -> GeminiArgsSpec:
    """Merge env and CLI specs with smart precedence rules.

    Rules:
    1. CLI subcommand takes precedence if present
    2. If CLI has positional args, they REPLACE all env positionals
    3. CLI flags override env flags (per-flag precedence)
    4. Repeatable flags are merged: ENV first, then CLI

    Returns:
        Merged GeminiArgsSpec with CLI taking precedence
    """
    # Determine subcommand (CLI wins if specified)
    final_subcommand = cli_spec.subcommand if cli_spec.subcommand else env_spec.subcommand

    # Handle positionals: CLI replaces env (if present)
    if cli_spec.positional_tokens:
        if cli_spec.positional_tokens == ("",):
            final_positionals = []
        else:
            final_positionals = list(cli_spec.positional_tokens)
    else:
        final_positionals = list(env_spec.positional_tokens)

    # Extract flag names from CLI
    cli_flag_names = _extract_flag_names_from_tokens(cli_spec.clean_tokens)

    # Build merged tokens using index-based filtering to avoid false collisions
    merged_tokens = []
    skip_next = False
    env_positional_indexes = set(env_spec.positional_indexes)

    for i, token in enumerate(env_spec.clean_tokens):
        if skip_next:
            skip_next = False
            continue

        # Skip positionals by index (not value, to avoid collisions)
        if i in env_positional_indexes:
            continue

        # Check if CLI overrides this flag
        flag_name = _extract_flag_name_from_token(token)
        if flag_name and flag_name in cli_flag_names:
            # For repeatable flags, we'll add both (env first, CLI later)
            if flag_name not in _REPEATABLE_FLAGS:
                if "=" not in token and i + 1 < len(env_spec.clean_tokens):
                    next_token = env_spec.clean_tokens[i + 1]
                    if not _looks_like_flag(next_token.lower()):
                        skip_next = True
                continue

        merged_tokens.append(token)

    # Append CLI tokens (excluding positionals by index)
    cli_positional_indexes = set(cli_spec.positional_indexes)
    for i, token in enumerate(cli_spec.clean_tokens):
        if i not in cli_positional_indexes:
            merged_tokens.append(token)

    # Deduplicate boolean flags
    merged_tokens = _deduplicate_boolean_flags(merged_tokens)

    # Insert positionals at end
    for pos in final_positionals:
        merged_tokens.append(pos)

    # Prepend subcommand
    combined: list[str] = []
    if final_subcommand:
        combined.append(final_subcommand)
    combined.extend(merged_tokens)

    return _parse_tokens(combined, "cli")


def validate_conflicts(spec: GeminiArgsSpec) -> list[str]:
    """Check for conflicting flag combinations.

    Returns list of error/warning messages. Items prefixed with "ERROR:" are
    hard errors that will cause Gemini CLI to fail.
    """
    warnings = []

    # Headless mode not supported in hcom (positional query or -p/--prompt)
    has_prompt_flag = spec.has_flag(["-p", "--prompt"], ("-p=", "--prompt="))
    if spec.positional_tokens:
        warnings.append(
            "ERROR: Gemini headless mode (positional query) not supported in hcom.\n"
            "       Use -i/--prompt-interactive for interactive sessions with initial prompt.\n"
            "       For headless: use 'hcom N claude -p \"task\"'"
        )
    elif has_prompt_flag:
        warnings.append(
            "ERROR: Gemini headless mode (-p/--prompt flag) not supported in hcom.\n"
            "       Use -i/--prompt-interactive for interactive sessions with initial prompt.\n"
            "       For headless: use 'hcom N claude -p \"task\"'"
        )

    # --yolo and --approval-mode together is redundant (CLI treats as error)
    if spec.is_yolo and spec.has_flag(["--approval-mode"], ("--approval-mode=",)):
        warnings.append("ERROR: --yolo and --approval-mode cannot be used together")

    # --prompt with positional prompt is invalid (already caught above, but keep for completeness)
    if has_prompt_flag and spec.positional_tokens:
        warnings.append("ERROR: --prompt cannot be used with a positional query argument")

    # --prompt with --prompt-interactive is invalid
    has_prompt_interactive = spec.has_flag(["-i", "--prompt-interactive"], ("-i=", "--prompt-interactive="))
    if has_prompt_flag and has_prompt_interactive:
        warnings.append("ERROR: --prompt and --prompt-interactive cannot be used together")

    # Validate --approval-mode enum values
    approval_value = spec.get_flag_value("--approval-mode")
    if (
        approval_value
        and isinstance(approval_value, str)
        and approval_value.lower() not in ("default", "auto_edit", "yolo", "plan")
    ):
        warnings.append(f"ERROR: invalid --approval-mode value '{approval_value}' (must be: default, auto_edit, yolo, plan)")

    # Validate --output-format enum values
    output_value = spec.get_flag_value("--output-format")
    if output_value and isinstance(output_value, str) and output_value.lower() not in ("text", "json", "stream-json"):
        warnings.append(f"ERROR: invalid --output-format value '{output_value}' (must be: text, json, stream-json)")

    return warnings


def _parse_tokens(
    tokens: Sequence[str],
    source: SourceType,
    initial_errors: Sequence[str] | None = None,
) -> GeminiArgsSpec:
    errors: list[str] = list(initial_errors or [])
    clean: TokenList = []
    positional: TokenList = []
    positional_indexes: list[int] = []
    flag_values: FlagValuesDict = {}

    subcommand: Subcommand = None
    is_headless: bool = False
    is_json: bool = False
    is_yolo: bool = False
    has_prompt_interactive: bool = False  # Track -i/--prompt-interactive
    output_format: OutputFormat = "text"
    approval_mode: ApprovalMode = "default"
    pending_flag: str | None = None
    after_double_dash: bool = False

    i: int = 0
    raw_tokens: TokenTuple = tuple(tokens)

    # Check for subcommand as first token
    if tokens and tokens[0].lower() in _SUBCOMMANDS:
        subcommand_str = tokens[0].lower()
        # Normalize aliases
        if subcommand_str in _SUBCOMMAND_ALIASES:
            subcommand_str = _SUBCOMMAND_ALIASES[subcommand_str]
        if subcommand_str in ("mcp", "extensions", "hooks", "skills"):
            subcommand = subcommand_str  # type: ignore
        i = 1

    while i < len(tokens):
        token = tokens[i]
        token_lower = token.lower()

        # Handle pending value flag
        if pending_flag:
            if _looks_like_flag(token_lower) and not after_double_dash:
                errors.append(f"{pending_flag} requires a value before '{token}'")
                pending_flag = None
                continue  # Don't advance, reprocess this token
            else:
                flag_key = pending_flag.lower()
                is_repeatable = flag_key in _REPEATABLE_FLAGS

                if is_repeatable:
                    if flag_key not in flag_values:
                        flag_values[flag_key] = []
                    flag_values[flag_key].append(token)  # type: ignore
                else:
                    flag_values[flag_key] = token

                # Check for headless indicators
                if pending_flag.lower() in ("-p", "--prompt"):
                    is_headless = True

                # Track --prompt-interactive (NOT headless)
                if pending_flag.lower() in ("-i", "--prompt-interactive"):
                    has_prompt_interactive = True

                # Check output format
                if pending_flag.lower() in ("-o", "--output-format"):
                    if token.lower() in ("json", "stream-json"):
                        is_json = True
                        output_format = token.lower()  # type: ignore
                    elif token.lower() == "text":
                        output_format = "text"

                # Check approval mode
                if pending_flag.lower() == "--approval-mode":
                    if token.lower() == "yolo":
                        is_yolo = True
                        approval_mode = "yolo"
                    elif token.lower() in ("default", "auto_edit", "plan"):
                        approval_mode = token.lower()  # type: ignore

                clean.append(token)
                pending_flag = None
                i += 1
                continue

        # After -- separator, everything is positional
        if after_double_dash:
            idx = len(clean)
            clean.append(token)
            positional.append(token)
            positional_indexes.append(idx)
            i += 1
            continue

        # Check for -- separator
        if token == "--":
            clean.append(token)
            after_double_dash = True
            i += 1
            continue

        # Check for boolean flags
        if token_lower in _BOOLEAN_FLAGS:
            clean.append(token)
            if token_lower in ("--yolo", "-y"):
                is_yolo = True
                approval_mode = "yolo"
            i += 1
            continue

        # Check for optional value flags (--resume, -r)
        # These can be used with or without a value
        if token_lower in _OPTIONAL_VALUE_FLAGS:
            clean.append(token)
            # Peek ahead - only consume value if it looks like a session ID
            # Session IDs are: numeric (1, 5, 123), "latest", or UUIDs
            if i + 1 < len(tokens):
                next_token = tokens[i + 1]
                next_lower = next_token.lower()
                if not _looks_like_flag(next_lower) and _looks_like_session_id(next_token):
                    # Has a session ID value, consume it
                    flag_values[token_lower] = next_token
                    clean.append(next_token)
                    i += 2
                    continue
            # No value or next is not a session ID - just the flag alone
            i += 1
            continue

        # Check for --flag=value syntax
        matched_prefix = None
        for prefix in _VALUE_FLAG_PREFIXES:
            if token_lower.startswith(prefix):
                matched_prefix = prefix
                break

        if matched_prefix:
            clean.append(token)
            flag_key = matched_prefix.rstrip("=")
            value = token[len(matched_prefix) :]

            if flag_key in _REPEATABLE_FLAGS:
                if flag_key not in flag_values:
                    flag_values[flag_key] = []
                flag_values[flag_key].append(value)  # type: ignore
            else:
                flag_values[flag_key] = value

            # Check for headless
            if flag_key in ("-p", "--prompt"):
                is_headless = True

            # Check for --prompt-interactive (NOT headless)
            if flag_key in ("-i", "--prompt-interactive"):
                has_prompt_interactive = True

            # Check output format
            if flag_key in ("-o", "--output-format"):
                if value.lower() in ("json", "stream-json"):
                    is_json = True
                    output_format = value.lower()  # type: ignore
                elif value.lower() == "text":
                    output_format = "text"

            # Check approval mode
            if flag_key == "--approval-mode":
                if value.lower() == "yolo":
                    is_yolo = True
                    approval_mode = "yolo"
                elif value.lower() in ("default", "auto_edit", "plan"):
                    approval_mode = value.lower()  # type: ignore

            i += 1
            continue

        # Check for value flags (space-separated)
        if token_lower in _VALUE_FLAGS:
            clean.append(token)
            pending_flag = token
            i += 1
            continue

        # Everything else is positional or unknown flag
        idx = len(clean)
        clean.append(token)
        if token_lower.startswith("--") or (
            token_lower.startswith("-") and len(token_lower) == 2 and token_lower[1].isalpha()
        ):
            if not _looks_like_flag(token_lower):
                base = token.split("=", 1)[0]
                suggested = difflib.get_close_matches(base, _KNOWN_GEMINI_FLAGS, n=1, cutoff=0.6)
                if suggested:
                    errors.append(
                        f"unknown option '{token}' (did you mean {suggested[0]}?). "
                        "If this was prompt text, pass '--' before it."
                    )
                else:
                    errors.append(f"unknown option '{token}'. If this was prompt text, pass '--' before it.")
                i += 1
                continue

        if not token_lower.startswith("-"):
            positional.append(token)
            positional_indexes.append(idx)
            # Positional prompt = headless mode (new Gemini CLI behavior)
            # UNLESS --prompt-interactive is set (interactive with initial message)
            if not has_prompt_interactive:
                is_headless = True
        i += 1

    # Check for dangling pending flag
    if pending_flag:
        errors.append(f"{pending_flag} requires a value at end of arguments")

    return GeminiArgsSpec(
        source=source,
        raw_tokens=raw_tokens,
        clean_tokens=tuple(clean),
        subcommand=subcommand,
        positional_tokens=tuple(positional),
        positional_indexes=tuple(positional_indexes),
        is_headless=is_headless,
        is_json=is_json,
        is_yolo=is_yolo,
        output_format=output_format,
        approval_mode=approval_mode,
        flag_values=dict(flag_values),
        errors=tuple(errors),
    )


def _looks_like_flag(token_lower: str) -> bool:
    """Check if token looks like a flag.

    Note: Subcommands (mcp, hooks, extensions) are NOT treated as flags here.
    They are only meaningful at position 0 and are valid values for flags
    like --model or --prompt (e.g., `gemini --prompt "explain mcp"`).
    """
    # Note: _SUBCOMMANDS intentionally NOT passed - they're valid flag values
    return _looks_like_flag_base(
        token_lower,
        boolean_flags=_BOOLEAN_FLAGS,
        value_flags=_VALUE_FLAGS,
        value_flag_prefixes=_VALUE_FLAG_PREFIXES,
        optional_value_flags=_OPTIONAL_VALUE_FLAGS,
    )


def _looks_like_session_id(token: str) -> bool:
    """Check if token looks like a Gemini session ID.

    Per gemini docs, session IDs can be:
    - Numeric index (1, 5, 123)
    - The word "latest"
    - A full UUID (a1b2c3d4-e5f6-7890-abcd-ef1234567890)

    Anything else (like "continue task") is treated as a positional prompt.
    """
    token_lower = token.lower()

    # "latest" keyword
    if token_lower == "latest":
        return True

    # Numeric index
    if token.isdigit():
        return True

    # UUID format (8-4-4-4-12 hex chars)
    uuid_pattern = r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"
    if re.match(uuid_pattern, token_lower):
        return True

    return False


def _deduplicate_boolean_flags(tokens: Sequence[str]) -> TokenList:
    """Remove duplicate boolean flags, keeping first occurrence."""
    return _deduplicate_boolean_flags_base(tokens, _BOOLEAN_FLAGS)
