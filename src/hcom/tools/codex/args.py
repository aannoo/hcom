#!/usr/bin/env python3
"""Codex CLI argument parsing and validation.

Parses Codex CLI arguments with support for:
- Subcommands (exec, resume, fork, review, mcp, sandbox, etc.)
- Flag aliases (-m/--model, -c/--config, etc.)
- Case-sensitive flags (-C/--cd vs -c/--config)
- Repeatable flags (-c can appear multiple times)
- Boolean flags (--json, --full-auto, etc.)

Key Semantic Flags:
    is_exec: True when subcommand is 'exec' or 'e'.
        hcom rejects exec mode - interactive mode works.
    is_json: True when --json flag present (JSONL output format).

Sandbox Considerations:
    Codex runs in a sandboxed environment by default. The preprocessing
    module handles --add-dir ~/.hcom to allow hcom DB writes.

Usage:
    >>> spec = resolve_codex_args(['--model', 'gpt-4'], None)
    >>> spec.get_flag_value('--model')
    'gpt-4'
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
    FlagValuesDict,
    extract_flag_names_from_tokens as _extract_flag_names_from_tokens,
    extract_flag_name_from_token as _extract_flag_name_from_token,
    deduplicate_boolean_flags as _deduplicate_boolean_flags_base,
    toggle_flag as _toggle_flag,
    looks_like_flag as _looks_like_flag_base,
    set_positional as _set_positional,
    remove_positional as _remove_positional,
)

# Type aliases (Codex-specific)
Subcommand: TypeAlias = Literal["exec", "resume", "fork", "review", None]

# ==================== Subcommand Classification ====================

# Subcommands that indicate exec mode (not supported in hcom)
_EXEC_SUBCOMMANDS: Final[frozenset[str]] = frozenset({"exec", "e"})  # e is alias for exec

# All known subcommands (from codex --help)
_SUBCOMMANDS: Final[frozenset[str]] = frozenset(
    {
        "exec",
        "e",  # e is alias for exec
        "resume",
        "fork",
        "review",
        "mcp",
        "mcp-server",
        "app-server",
        "login",
        "logout",
        "completion",
        "sandbox",
        "debug",  # debugging tools (separate from sandbox)
        "apply",
        "app",
        "a",  # a is alias for apply
        "cloud",
        "features",
        "help",
    }
)

# ==================== Flag Classification ====================

# Flag aliases: map short forms to canonical long forms
# Keys and values are lowercase
_FLAG_ALIASES: Final[Mapping[str, str]] = {
    "-m": "--model",
    "-c": "--config",
    "-i": "--image",
    "-p": "--profile",
    "-s": "--sandbox",
    "-a": "--ask-for-approval",
    "-o": "--output-last-message",
    # Note: -C (cd) is NOT aliased to -c (config) - they are different!
}

# Canonical forms for case-sensitive flags
# -C is --cd (uppercase C), -c is --config (lowercase c)
_CASE_SENSITIVE_FLAGS: Final[Mapping[str, str]] = {"-C": "--cd", "-c": "--config"}

# Case-sensitive boolean flags (must match exactly, not lowercased)
# -V is uppercase for version (Codex CLI rejects lowercase -v)
_CASE_SENSITIVE_BOOLEAN_FLAGS: Final[frozenset[str]] = frozenset({"-V"})

# Boolean flags (no value required)
# Note: Use lowercase for short flags since matching uses token.lower()
_BOOLEAN_FLAGS: Final[frozenset[str]] = frozenset(
    {
        # Global
        "--oss",
        "--full-auto",
        "--dangerously-bypass-approvals-and-sandbox",
        "--search",
        "--no-alt-screen",
        "-h",
        "--help",
        "--version",  # Note: -V is handled case-sensitively in _CASE_SENSITIVE_BOOLEAN_FLAGS
        # Exec-specific
        "--skip-git-repo-check",
        "--json",
        # Resume-specific
        "--last",
        "--all",
        # Review-specific
        "--uncommitted",
    }
)

# Flags that require a following value (lowercase for matching, except case-sensitive ones)
_VALUE_FLAGS: Final[frozenset[str]] = frozenset(
    {
        # Global (short and long forms)
        "-c",
        "--config",
        "--enable",
        "--disable",
        "-i",
        "--image",
        "-m",
        "--model",
        "--local-provider",
        "-p",
        "--profile",
        "-s",
        "--sandbox",
        "-a",
        "--ask-for-approval",
        "--cd",  # Long form of -C
        "--add-dir",
        # Exec-specific
        "--color",
        "-o",
        "--output-last-message",
        "--output-schema",
        # Review-specific
        "--base",
        "--commit",
        "--title",
    }
)

# Case-sensitive value flags (must match exactly)
_CASE_SENSITIVE_VALUE_FLAGS: Final[frozenset[str]] = frozenset({"-C", "-c"})

# Flags with = syntax prefixes (lowercase for matching)
_VALUE_FLAG_PREFIXES: Final[frozenset[str]] = frozenset(
    {
        "-c=",
        "--config=",
        "--enable=",
        "--disable=",
        "-i=",
        "--image=",
        "-m=",
        "--model=",
        "--local-provider=",
        "-p=",
        "--profile=",
        "-s=",
        "--sandbox=",
        "-a=",
        "--ask-for-approval=",
        "--cd=",
        "--add-dir=",
        "--color=",
        "-o=",
        "--output-last-message=",
        "--output-schema=",
        "--base=",
        "--commit=",
        "--title=",
    }
)

# Used for friendly "did you mean" suggestions when user typos a flag.
_KNOWN_CODEX_OPTIONS: Final[list[str]] = sorted(
    {
        *_BOOLEAN_FLAGS,
        *_CASE_SENSITIVE_BOOLEAN_FLAGS,
        *_VALUE_FLAGS,
        *_CASE_SENSITIVE_VALUE_FLAGS,
        *_SUBCOMMANDS,
        *_FLAG_ALIASES.keys(),
        *_FLAG_ALIASES.values(),
    }
)

# Case-sensitive prefixes
_CASE_SENSITIVE_PREFIXES: Final[frozenset[str]] = frozenset({"-C=", "-c="})

# Repeatable flags (can appear multiple times)
_REPEATABLE_FLAGS: Final[frozenset[str]] = frozenset(
    {
        "-c",
        "--config",
        "--enable",
        "--disable",
        "-i",
        "--image",
        "--add-dir",
    }
)


@dataclass(frozen=True)
class CodexArgsSpec(BaseArgsSpec):
    """Normalized representation of Codex CLI arguments.

    Inherits common fields and methods from BaseArgsSpec.
    Key fields for launch logic:
    - is_exec: True = exec subcommand detected. Used to REJECT such launches
      (Codex exec/headless mode is not supported in hcom).
    - is_json: True = JSONL output format (--json flag)
    """

    subcommand: Subcommand = None
    is_json: bool = False  # --json flag: JSONL output format
    is_exec: bool = False  # exec subcommand: detected for rejection (not supported in hcom)

    def get_flag_value(self, flag_name: str) -> str | list[str] | None:
        """Get value of a flag.

        For repeatable flags (-c, --config, etc.), returns list.
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

        # Handle case-sensitive flags (-C vs -c)
        if flag_name in _CASE_SENSITIVE_FLAGS:
            # Use the canonical form for lookup
            canonical = _CASE_SENSITIVE_FLAGS[flag_name]
            possible_flags.add(canonical)
            possible_flags.add(flag_name)  # Keep original case

        # Check pre-parsed flag_values (already has "last wins" from parsing)
        for pf in possible_flags:
            if pf in self.flag_values:
                return self.flag_values[pf]

        # Fallback: scan clean_tokens once in order, checking both forms
        # This preserves chronological order for "last wins" semantics
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
                if token_lower in possible_flags:
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
        """Return token list suitable for invoking Codex.

        Overrides base class to prepend subcommand if present.

        Args:
            include_positionals: Include positional arguments (inherited from base)
            include_subcommand: Include subcommand prefix (Codex-specific)
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
        prompt: str | None = None,
        subcommand: Subcommand | str = None,
        developer_instructions: str | None = None,
    ) -> "CodexArgsSpec":
        """Return new spec with requested updates applied.

        Args:
            json_output: Set --json flag (JSONL output format)
            prompt: Set positional prompt argument
            subcommand: Set subcommand (exec/resume/review)
            developer_instructions: Custom instructions content. Generates
                `-c developer_instructions=<content>` flag.
                Uses developer role message (works with GPT-5 models).
                PREPENDED to existing flags (takes precedence).
        """
        tokens = list(self.clean_tokens)
        new_subcommand: str | None = self.subcommand

        if subcommand is not None:
            new_subcommand = subcommand if subcommand else None

        if json_output is not None:
            tokens = _toggle_flag(tokens, "--json", json_output)

        if prompt is not None:
            if prompt == "":
                tokens = _remove_positional(tokens, self.positional_indexes)
            else:
                tokens = _set_positional(tokens, prompt, self.positional_indexes)

        # Developer instructions via -c flag - PREPEND for precedence
        if developer_instructions is not None:
            tokens = ["-c", f"developer_instructions={developer_instructions}"] + tokens

        # Rebuild with subcommand
        combined: list[str] = []
        if new_subcommand:
            combined.append(new_subcommand)
        combined.extend(tokens)

        return _parse_tokens(combined, self.source)


def resolve_codex_args(
    cli_args: Sequence[str] | None,
    env_value: str | None,
) -> CodexArgsSpec:
    """Resolve Codex args from CLI (highest precedence) or env string."""
    if cli_args:
        return _parse_tokens(cli_args, "cli")

    if env_value is not None:
        try:
            tokens = shlex.split(env_value)
        except ValueError as err:
            return _parse_tokens([], "env", initial_errors=[f"invalid Codex args: {err}"])
        return _parse_tokens(tokens, "env")

    return _parse_tokens([], "none")


def merge_codex_args(env_spec: CodexArgsSpec, cli_spec: CodexArgsSpec) -> CodexArgsSpec:
    """Merge env and CLI specs with smart precedence rules.

    Rules:
    1. CLI subcommand takes precedence if present
    2. If CLI has positional args, they REPLACE all env positionals
    3. CLI flags override env flags (per-flag precedence)
    4. Repeatable flags (-c, --config, etc.) are merged: ENV first, then CLI
       Order: [env -c flags] [cli -c flags]
    5. Sandbox flags are treated as a GROUP: if CLI has ANY sandbox-related flag,
       ALL sandbox flags are stripped from env (--sandbox, -a, --full-auto,
       --dangerously-bypass-approvals-and-sandbox)

    For system prompt injection, use spec.update(system_prompt_file=...) or
    prepend_system_prompt() AFTER merging - system prompt -c flags should
    be PREPENDED to take precedence over user -c flags.

    Returns:
        Merged CodexArgsSpec with CLI taking precedence
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

    # Sandbox flags are a GROUP: if CLI overrides any, strip all from env
    # This prevents conflicts like "--sandbox workspace-write -a untrusted --full-auto"
    sandbox_flags = {
        "--sandbox",
        "-s",
        "-a",
        "--ask-for-approval",
        "--full-auto",
        "--dangerously-bypass-approvals-and-sandbox",
    }
    cli_has_sandbox = bool(cli_flag_names & sandbox_flags)
    if cli_has_sandbox:
        cli_flag_names = cli_flag_names | sandbox_flags  # Treat all sandbox flags as overridden

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

    # Build combined args list
    # For resume subcommand: positionals (thread-id) must come IMMEDIATELY after "resume"
    # For other subcommands: positionals go at the end
    combined: list[str] = []
    if final_subcommand:
        combined.append(final_subcommand)
        if final_subcommand == "resume":
            # Insert positionals right after subcommand for resume
            combined.extend(final_positionals)
            combined.extend(merged_tokens)
        else:
            combined.extend(merged_tokens)
            combined.extend(final_positionals)
    else:
        combined.extend(merged_tokens)
        combined.extend(final_positionals)

    return _parse_tokens(combined, "cli")


def validate_conflicts(spec: CodexArgsSpec) -> list[str]:
    """Check for conflicting flag combinations.

    Returns list of error/warning messages. Items prefixed with "ERROR:" are
    hard errors that will cause launch to fail.
    """
    warnings = []

    # Exec mode not supported in hcom (requires PTY interaction)
    if spec.is_exec:
        warnings.append(
            "ERROR: Codex exec mode not supported in hcom.\n"
            "       Use interactive mode (no 'exec' subcommand) for PTY sessions.\n"
            "       For headless: use 'hcom N claude -p \"task\"'"
        )

    # --json only valid with exec
    if spec.is_json and not spec.is_exec:
        warnings.append("--json flag is only valid with 'exec' subcommand")

    # --full-auto and --dangerously-bypass conflict
    if spec.has_flag(["--full-auto"]) and spec.has_flag(["--dangerously-bypass-approvals-and-sandbox"]):
        warnings.append("--full-auto and --dangerously-bypass-approvals-and-sandbox are redundant together")

    return warnings


def _parse_tokens(
    tokens: Sequence[str],
    source: SourceType,
    initial_errors: Sequence[str] | None = None,
) -> CodexArgsSpec:
    errors: list[str] = list(initial_errors or [])
    clean: TokenList = []
    positional: TokenList = []
    positional_indexes: list[int] = []
    flag_values: FlagValuesDict = {}

    subcommand: Subcommand = None
    is_json: bool = False
    pending_flag: str | None = None
    after_double_dash: bool = False

    i: int = 0
    raw_tokens: TokenTuple = tuple(tokens)

    # Check for subcommand as first token
    if tokens and tokens[0].lower() in _SUBCOMMANDS:
        subcommand_str = tokens[0].lower()
        # Map aliases to canonical names
        if subcommand_str == "e":
            subcommand_str = "exec"
        if subcommand_str in ("exec", "resume", "fork", "review"):
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
                # Store the value - preserve case for case-sensitive flags
                if pending_flag in _CASE_SENSITIVE_VALUE_FLAGS:
                    flag_key = pending_flag  # Keep original case (-C vs -c)
                    # Check if this specific flag (case-sensitive) is repeatable
                    is_repeatable = pending_flag.lower() in _REPEATABLE_FLAGS and pending_flag == pending_flag.lower()
                else:
                    flag_key = pending_flag.lower()
                    is_repeatable = flag_key in _REPEATABLE_FLAGS

                if is_repeatable:
                    if flag_key not in flag_values:
                        flag_values[flag_key] = []
                    flag_values[flag_key].append(token)  # type: ignore
                else:
                    flag_values[flag_key] = token
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

        # Check for case-sensitive boolean flags first (e.g., -V for version)
        if token in _CASE_SENSITIVE_BOOLEAN_FLAGS:
            clean.append(token)
            i += 1
            continue

        # Check for boolean flags (lowercase matching)
        if token_lower in _BOOLEAN_FLAGS:
            clean.append(token)
            if token_lower == "--json":
                is_json = True
            i += 1
            continue

        # Check for case-sensitive --flag=value syntax first (-C= vs -c=)
        matched_prefix = None
        for prefix in _CASE_SENSITIVE_PREFIXES:
            if token.startswith(prefix):  # Use original token, not lowercased
                matched_prefix = prefix
                break

        # Then check regular prefixes (lowercase matching)
        if not matched_prefix:
            for prefix in _VALUE_FLAG_PREFIXES:
                if token_lower.startswith(prefix):
                    matched_prefix = prefix
                    break

        if matched_prefix:
            clean.append(token)
            # For case-sensitive prefixes, preserve the flag case
            flag_key = matched_prefix.rstrip("=")
            value = token[len(matched_prefix) :]
            if flag_key in _REPEATABLE_FLAGS:
                if flag_key not in flag_values:
                    flag_values[flag_key] = []
                flag_values[flag_key].append(value)  # type: ignore
            else:
                flag_values[flag_key] = value
            i += 1
            continue

        # Check for case-sensitive value flags first (-C vs -c)
        if token in _CASE_SENSITIVE_VALUE_FLAGS:
            clean.append(token)
            pending_flag = token
            i += 1
            continue

        # Check for value flags (space-separated, case-insensitive)
        if token_lower in _VALUE_FLAGS:
            clean.append(token)
            pending_flag = token
            i += 1
            continue

        # Everything else is positional or unknown flag
        if token_lower.startswith("-") and not _looks_like_flag(token_lower):
            base = token.split("=", 1)[0]
            suggested = difflib.get_close_matches(base, _KNOWN_CODEX_OPTIONS, n=1, cutoff=0.6)
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
        if not token_lower.startswith("-"):
            positional.append(token)
            positional_indexes.append(idx)
        i += 1

    # Check for dangling pending flag
    if pending_flag:
        errors.append(f"{pending_flag} requires a value at end of arguments")

    is_exec = subcommand in _EXEC_SUBCOMMANDS

    return CodexArgsSpec(
        source=source,
        raw_tokens=raw_tokens,
        clean_tokens=tuple(clean),
        subcommand=subcommand,
        positional_tokens=tuple(positional),
        positional_indexes=tuple(positional_indexes),
        is_json=is_json,
        is_exec=is_exec,
        flag_values=dict(flag_values),
        errors=tuple(errors),
    )


def _looks_like_flag(token_lower: str) -> bool:
    """Check if token looks like a flag.

    Note: Case-sensitive boolean flags like -V are NOT checked here because
    this function receives lowercased tokens. The parsing loop handles -V
    case-sensitively before calling _looks_like_flag.
    """
    return _looks_like_flag_base(
        token_lower,
        boolean_flags=_BOOLEAN_FLAGS,
        value_flags=_VALUE_FLAGS,
        value_flag_prefixes=_VALUE_FLAG_PREFIXES,
        extra_flags=_SUBCOMMANDS,
    )


def _deduplicate_boolean_flags(tokens: Sequence[str]) -> TokenList:
    """Remove duplicate boolean flags, keeping first occurrence."""
    return _deduplicate_boolean_flags_base(tokens, _BOOLEAN_FLAGS)


# ==================== Codex Args Preprocessing ====================
# Re-export from preprocessing.py (canonical location) for backwards compatibility
