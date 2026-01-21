"""Codex-specific runtime preprocessing for hcom integration.

Transforms Codex CLI arguments before launch to enable hcom functionality
within Codex's sandboxed environment.

Preprocessing Steps:
    1. Sandbox flags: --sandbox workspace-write (or per mode)
    2. DB access: --add-dir ~/.hcom (allows hcom writes from sandbox)
    3. Bootstrap: -c developer_instructions=<bootstrap text>

Sandbox Modes:
    workspace: Auto-approve edits in workspace (default for hcom)
    untrusted: All edits need Y/n approval, but hcom still works
    danger-full-access: No sandbox restrictions
    none: Raw codex, user's own settings (hcom may not work)

Key Functions:
    preprocess_codex_args: Apply all preprocessing steps
    get_sandbox_flags: Get flags for a given sandbox mode
    ensure_hcom_writable: Add --add-dir ~/.hcom if sandbox active
    add_codex_developer_instructions: Inject bootstrap via -c flag

Note: This module is separate from args.py (pure parsing) to keep
concerns distinct. args.py handles validation, this handles mutation.
"""

from __future__ import annotations


def get_sandbox_flags(mode: str) -> list[str]:
    """Get sandbox flags for the given mode.

    Modes:
    - workspace: Normal codex - edits auto-approved in workspace (default)
    - untrusted: Read-only - all edits need Y/n approval, but hcom works
    - danger-full-access: Full access (no sandbox restrictions)
    - none: No flags - raw codex folder trust (hcom may not work)
    """
    return {
        "workspace": ["--sandbox", "workspace-write"],
        "untrusted": ["--sandbox", "workspace-write", "-a", "untrusted"],
        "danger-full-access": ["--sandbox", "danger-full-access"],
        "none": [],
    }.get(mode, ["--sandbox", "workspace-write"])  # default to workspace


def ensure_hcom_writable(tokens: list[str]) -> list[str]:
    """Ensure --add-dir ~/.hcom is present so hcom can write to its DB.

    Codex's --add-dir flag is IGNORED in read-only sandbox mode, but required
    for workspace-write mode to allow hcom DB writes.

    If no sandbox flags are present (mode="none"), skip adding --add-dir
    since user is using codex's own folder settings and takes responsibility
    for hcom DB access.
    """
    from ...core.paths import hcom_path
    from .args import resolve_codex_args

    spec = resolve_codex_args(tokens, None)

    # If no sandbox flags, assume mode="none" - skip --add-dir
    # User takes responsibility for hcom DB access
    has_sandbox = spec.has_flag(
        ["--sandbox", "-s", "--dangerously-bypass-approvals-and-sandbox"],
        ["--sandbox=", "-s="],
    )
    if not has_sandbox:
        return tokens  # Mode is "none", user's own settings

    hcom_dir = str(hcom_path())

    # Check if --add-dir with hcom path already exists
    for i, token in enumerate(spec.clean_tokens):
        if token == "--add-dir" and i + 1 < len(spec.clean_tokens):
            if spec.clean_tokens[i + 1] == hcom_dir:
                return tokens  # Already present

    # Add --add-dir at the beginning
    return ["--add-dir", hcom_dir] + tokens


def add_codex_developer_instructions(codex_args: list[str], instance_name: str) -> list[str]:
    """Add hcom bootstrap to codex developer_instructions.

    Builds full bootstrap and adds via -c developer_instructions=... flag.
    If user also provided developer_instructions (via system_prompt in launcher),
    bootstrap comes first, then separator, then user content below.

    Skip for resume/review subcommands (not interactive launch).
    """
    from ...core.bootstrap import get_bootstrap
    from .args import resolve_codex_args

    spec = resolve_codex_args(codex_args, None)

    # Skip non-interactive modes and resume/fork (already has bootstrap from original session)
    if spec.subcommand in ("exec", "e", "resume", "fork", "review"):
        return list(codex_args)

    # Build bootstrap for this instance
    bootstrap = get_bootstrap(instance_name, tool="codex")

    # Check if developer_instructions already exists in -c flags
    existing_dev_instructions: str | None = None
    tokens = list(spec.clean_tokens)

    # Scan for -c developer_instructions=... pattern
    i = 0
    dev_instr_idx: int | None = None
    while i < len(tokens):
        token = tokens[i]
        # Handle -c developer_instructions=value (equals syntax)
        if token.startswith("-c=developer_instructions=") or token.startswith("--config=developer_instructions="):
            existing_dev_instructions = token.split("=", 2)[2] if token.count("=") >= 2 else ""
            dev_instr_idx = i
            break
        # Handle -c developer_instructions=value (space syntax)
        if token in ("-c", "--config") and i + 1 < len(tokens):
            next_token = tokens[i + 1]
            if next_token.startswith("developer_instructions="):
                existing_dev_instructions = next_token.split("=", 1)[1]
                dev_instr_idx = i
                break
        i += 1

    # Build combined developer instructions
    if existing_dev_instructions:
        # Bootstrap first, then user content below separator
        combined = f"{bootstrap}\n---\n{existing_dev_instructions}"
        # Remove existing developer_instructions from tokens
        if dev_instr_idx is not None:
            if tokens[dev_instr_idx] in ("-c", "--config"):
                # Remove both -c and the value
                tokens = tokens[:dev_instr_idx] + tokens[dev_instr_idx + 2 :]
            else:
                # Remove single equals-style token
                tokens = tokens[:dev_instr_idx] + tokens[dev_instr_idx + 1 :]
    else:
        # No existing - just use bootstrap
        combined = bootstrap

    # Prepend -c developer_instructions=... to tokens
    result_tokens = ["-c", f"developer_instructions={combined}"] + tokens

    # Prepend subcommand if present
    if spec.subcommand:
        result_tokens = [spec.subcommand] + result_tokens

    return result_tokens


def preprocess_codex_args(
    codex_args: list[str],
    instance_name: str,
    sandbox_mode: str = "workspace",
) -> list[str]:
    """Preprocess Codex CLI arguments for hcom integration.

    Applies:
    1. Sandbox flags based on mode
    2. --add-dir ~/.hcom for hcom DB writes
    3. Bootstrap injection via developer_instructions

    This function should be called BEFORE launching Codex PTY.

    Args:
        codex_args: Raw codex CLI arguments
        instance_name: Instance name for bootstrap
        sandbox_mode: Sandbox mode (workspace, untrusted, danger-full-access, none)

    Returns:
        Preprocessed arguments ready for Codex launch
    """
    import sys

    # 1. Inject sandbox flags based on mode
    sandbox_flags = get_sandbox_flags(sandbox_mode)
    if sandbox_flags:
        codex_args = sandbox_flags + list(codex_args or [])
    else:
        codex_args = list(codex_args or [])

    # Warn if mode is "none" (no sandbox flags = hcom likely broken)
    if sandbox_mode == "none":
        print(
            "[hcom] Warning: Sandbox mode is 'none' - --add-dir ~/.hcom disabled.",
            file=sys.stderr,
        )
        print(
            "[hcom] hcom commands may fail unless HCOM_DIR is within workspace.",
            file=sys.stderr,
        )

    # 2. Ensure --add-dir ~/.hcom is present for hcom DB writes (skips if mode="none")
    codex_args = ensure_hcom_writable(codex_args)

    # 3. Add bootstrap to developer_instructions
    codex_args = add_codex_developer_instructions(codex_args, instance_name)

    return codex_args


__all__ = [
    "get_sandbox_flags",
    "ensure_hcom_writable",
    "add_codex_developer_instructions",
    "preprocess_codex_args",
]
