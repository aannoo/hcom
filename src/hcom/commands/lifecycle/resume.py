"""Resume and fork commands."""

from ..utils import CLIError, resolve_identity
from ...shared import IS_WINDOWS
from ...core.thread_context import get_cwd
from ...core.instances import load_instance_position, update_instance_position


def _load_stopped_snapshot(name: str) -> dict | None:
    """Load instance snapshot from stopped events."""
    from ...core.ops import load_stopped_snapshot
    return load_stopped_snapshot(name)


def _parse_extra_args(tool: str, extra_args: list[str]) -> tuple[bool | None, list[str]]:
    """Parse extra tool flags, return (headless_override, validated tokens).

    Returns headless_override=True/False if extra args explicitly set headless mode,
    or None if they don't affect it. validated tokens are the clean_tokens from parsing.
    """
    if not extra_args:
        return None, []

    if tool == "claude":
        from ...tools.claude.args import resolve_claude_args
        claude_spec = resolve_claude_args(extra_args, None)
        if claude_spec.errors:
            raise CLIError(f"Bad flag: {claude_spec.errors[0]}")
        headless = True if claude_spec.is_background else None
        return headless, list(claude_spec.clean_tokens)
    elif tool == "gemini":
        from ...tools.gemini.args import resolve_gemini_args
        gemini_spec = resolve_gemini_args(extra_args, None)
        if gemini_spec.errors:
            raise CLIError(f"Bad flag: {gemini_spec.errors[0]}")
        headless = True if gemini_spec.is_headless else None
        return headless, list(gemini_spec.clean_tokens)
    elif tool == "codex":
        from ...tools.codex.args import resolve_codex_args
        codex_spec = resolve_codex_args(extra_args, None)
        if codex_spec.errors:
            raise CLIError(f"Bad flag: {codex_spec.errors[0]}")
        return None, list(codex_spec.clean_tokens)
    else:
        return None, list(extra_args)


def _do_resume(name: str, prompt: str | None = None, *, run_here: bool | None = None, fork: bool = False, extra_args: list[str] | None = None) -> int:
    """Resume or fork an instance by launching tool with --resume and session_id.

    Used by TUI [R] key, `hcom r NAME`, and `hcom f NAME`.

    Args:
        name: Instance name to resume/fork.
        prompt: Optional prompt to pass to instance.
        run_here: If False, force new terminal window. If None, use default logic.
        fork: If True, fork the session (new instance) instead of resuming.
        extra_args: Additional tool flags (e.g. --model opus) to pass through.
    """
    from ...launcher import launch as unified_launch

    # Look up instance data — fork allows active, resume requires stopped
    active_data = load_instance_position(name)
    if fork:
        # Fork works on active or stopped
        if active_data:
            instance_data = dict(active_data)
        else:
            stopped_data = _load_stopped_snapshot(name)
            if not stopped_data:
                raise CLIError(f"'{name}' not found (not active or stopped)")
            instance_data = stopped_data
    else:
        # Resume requires stopped
        if active_data:
            raise CLIError(f"'{name}' is still active — run hcom stop {name} first")
        stopped_data = _load_stopped_snapshot(name)
        if not stopped_data:
            raise CLIError(f"'{name}' not found in stopped instances")
        instance_data = stopped_data

    session_id = instance_data.get("session_id")
    if not session_id:
        raise CLIError(f"'{name}' has no session_id (cannot {'fork' if fork else 'resume'})")

    tool = instance_data.get("tool", "claude")
    if fork and tool not in ("claude", "codex"):
        raise CLIError(f"Fork not supported for {tool}")

    # Parse extra flags through tool's arg parser for validation + headless detection
    headless_override, extra_tokens = _parse_extra_args(tool, extra_args or [])

    is_headless = headless_override if headless_override is not None else bool(instance_data.get("background", False))
    original_tag = instance_data.get("tag") or None
    original_dir = instance_data.get("directory") or str(get_cwd())

    # System prompt
    from ...core.ops import resume_system_prompt
    system_prompt = resume_system_prompt(name, fork=fork)

    # Build args
    if tool == "claude":
        args = ["--resume", session_id]
        if fork:
            args.append("--fork-session")
        if is_headless and "-p" not in extra_tokens:
            args.append("-p")
        if prompt:
            args.append(prompt)
    elif tool == "gemini":
        args = ["--resume", session_id]
    elif tool == "codex":
        args = ["fork" if fork else "resume", session_id]
    else:
        raise CLIError(f"{'Fork' if fork else 'Resume'} not supported for tool: {tool}")

    # Append extra tool flags
    args.extend(extra_tokens)

    # Get launcher name
    try:
        launcher_name = resolve_identity().name
    except Exception:
        launcher_name = "user"

    # PTY mode: use PTY wrapper for interactive Claude (not headless, not Windows)
    use_pty = tool == "claude" and not is_headless and not IS_WINDOWS

    # Launch in original directory
    # For resume (not fork), reuse the original instance name
    result = unified_launch(
        tool,
        1,
        args,
        launcher=launcher_name,
        background=is_headless,
        system_prompt=system_prompt,
        prompt=prompt if is_headless else None,
        run_here=run_here,
        cwd=original_dir,
        pty=use_pty,
        name=name if not fork else None,
        tag=original_tag,
    )

    launched = result["launched"]
    if launched == 1:
        # Restore cursor so messages between stop and resume are delivered
        if not fork:
            stopped_cursor = instance_data.get("last_event_id")
            if stopped_cursor is not None:
                update_instance_position(name, {"last_event_id": stopped_cursor})
        print(f"{'Forked' if fork else 'Resumed'} {name} ({tool})")
        return 0
    else:
        return 1


def cmd_resume(argv: list[str], *, ctx=None) -> int:
    """Resume a stopped instance: hcom r NAME [tool flags...]"""
    if not argv or argv[0] in ("--help", "-h"):
        print("Usage: hcom r NAME [flags...]")
        print("Resume a stopped agent session")
        print("Extra flags (e.g. --model opus) are passed to the tool")
        return 0
    name = argv[0]
    return _do_resume(name, extra_args=argv[1:])


def cmd_fork(argv: list[str], *, ctx=None) -> int:
    """Fork an agent session: hcom f NAME [tool flags...]"""
    if not argv or argv[0] in ("--help", "-h"):
        print("Usage: hcom f NAME [flags...]")
        print("Fork an agent session (active or stopped) into a new instance")
        print("Extra flags (e.g. --model opus) are passed to the tool")
        return 0
    name = argv[0]
    return _do_resume(name, fork=True, extra_args=argv[1:])
