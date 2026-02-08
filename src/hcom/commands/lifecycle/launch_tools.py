"""Launch Gemini and Codex instances."""

import sys
import time

from ..utils import (
    CLIError,
    is_interactive,
    resolve_identity,
)
from ...shared import (
    IS_WINDOWS,
    is_inside_ai_tool,
    CommandContext,
)
from ...core.thread_context import get_hcom_go
from ...core.paths import hcom_path
from .launch import _print_launch_preview


def cmd_launch_gemini(
    argv: list[str],
    *,
    launcher_name: str | None = None,
    ctx: "CommandContext | None" = None,
) -> int:
    """Launch Gemini instances: hcom <N> gemini [gemini-args...]

    Args:
        argv: Command line arguments (identity flags already stripped)
        launcher_name: Explicit launcher identity from --name flag (CLI layer parsed this)
        ctx: Command context with explicit_name if --name was provided

    Examples:
        hcom 1 gemini                    # Launch 1 Gemini instance (interactive)
        hcom 2 gemini                    # Launch 2 Gemini instances
        hcom 1 gemini -i "task"          # Interactive with initial prompt
        hcom 1 gemini --resume latest    # Resume latest Gemini session (interactive)

    Note: Gemini headless mode not supported. Use claude or codex for headless.

    Raises:
        HcomError: On hook setup failure or launch failure.
    """
    # Platform check - Gemini PTY requires Unix-only APIs
    if IS_WINDOWS:
        raise CLIError(
            "Gemini CLI integration requires PTY (pseudo-terminal) which is not available on Windows.\n"
            "Use 'hcom N claude' for Claude Code on Windows (hooks-based, no PTY required)."
        )

    from ...launcher import launch as unified_launch
    from ...core.config import get_config

    # Hook setup + version check moved to launcher.launch() - single source of truth

    # Parse count (required first arg)
    if not argv or not argv[0].isdigit():
        raise CLIError("Usage: hcom <N> gemini [gemini-args...]")

    count = int(argv[0])
    if count <= 0:
        raise CLIError("Count must be positive.")
    if count > 10:
        raise CLIError("Too many Gemini agents (max 10).")
    argv = argv[1:]

    # Skip 'gemini' keyword
    if argv and argv[0] == "gemini":
        argv = argv[1:]

    # Note: Identity flags (--name) already stripped by CLI layer

    # Parse using proper Gemini args parser - merge env (HCOM_GEMINI_ARGS) and CLI args
    from ...tools.gemini.args import resolve_gemini_args, merge_gemini_args

    env_spec = resolve_gemini_args(None, get_config().gemini_args)
    cli_spec = resolve_gemini_args(argv, None)
    spec = merge_gemini_args(env_spec, cli_spec) if (cli_spec.clean_tokens or cli_spec.positional_tokens) else env_spec

    # Validate parsed args (strict by default, overridable).
    from ...shared import skip_tool_args_validation, HCOM_SKIP_TOOL_ARGS_VALIDATION_ENV

    if spec.has_errors() and not skip_tool_args_validation():
        raise CLIError(
            "\n".join(
                [
                    *spec.errors,
                    f"Tip: set {HCOM_SKIP_TOOL_ARGS_VALIDATION_ENV}=1 to bypass hcom validation and let gemini handle args.",
                ]
            )
        )

    # Reject headless mode (positional query or -p/--prompt flag)
    if spec.positional_tokens or spec.has_flag(["-p", "--prompt"], ("-p=", "--prompt=")):
        headless_type = "positional query" if spec.positional_tokens else "-p/--prompt flag"
        raise CLIError(
            f"Gemini headless mode not supported in hcom (attempted: {headless_type}).\n"
            "  • For interactive: hcom N gemini\n"
            '  • For interactive with initial prompt: hcom N gemini -i "prompt"\n'
            '  • For headless: hcom N claude -p "task"'
        )

    # Launch confirmation gate: inside AI tools, require HCOM_GO=1
    # Show preview if: has args OR count > 5
    has_args = argv and len(argv) > 0
    if is_inside_ai_tool() and not get_hcom_go() and (has_args or count > 5):
        _print_launch_preview("gemini", count, False, argv)  # Gemini always interactive
        return 0

    # Check for --no-auto-watch flag (used by TUI to prevent opening another watch window)
    no_auto_watch = "--no-auto-watch" in argv
    if no_auto_watch:
        argv = [arg for arg in argv if arg != "--no-auto-watch"]

    # Build final args from merged spec
    gemini_args = spec.rebuild_tokens()

    # Determine if instance will run in current terminal (blocking mode)
    from ...launcher import will_run_in_current_terminal

    ran_here = will_run_in_current_terminal(count, False)  # Gemini always interactive

    # Resolve launcher identity: use explicit --name if provided, else auto-resolve
    if not launcher_name:
        try:
            launcher_name = resolve_identity().name
        except Exception:
            launcher_name = None  # Let unified_launch handle fallback

    result = unified_launch(
        "gemini",
        count,
        gemini_args,
        launcher=launcher_name,
        background=False,  # Gemini headless not supported
        system_prompt=get_config().gemini_system_prompt or None,
    )

    # Surface per-instance launch errors
    for err in result.get("errors", []):
        error_msg = err.get("error", "Unknown error")
        print(f"Error: {error_msg}", file=sys.stderr)

    launched = result["launched"]
    failed = result["failed"]

    if launched == 0 and failed > 0:
        return 1  # All failed, exit with error

    instance_names: list[str] = []
    for h in result.get("handles", []):
        name = h.get("instance_name")
        if name:
            instance_names.append(name)
            print(f"Started the launch process for Gemini: {name}")

    print(f"\nStarted the launch process for {launched} Gemini agent{'s' if launched != 1 else ''}")
    print(f"Batch id: {result['batch_id']}")
    print("To block until ready, fail, or timeout (30s), run: hcom events launch")

    # Auto-launch TUI if:
    # - Not print mode, not auto-watch disabled, all launched, interactive terminal
    # - Did NOT run in current terminal (ran_here=True means single instance already finished)
    # - NOT inside AI tool (would hijack the session)
    # - NOT ad-hoc launch with --name (external script doesn't want TUI)
    terminal_mode = get_config().terminal
    explicit_name_provided = ctx and ctx.explicit_name
    if (
        terminal_mode != "print"
        and failed == 0
        and is_interactive()
        and not no_auto_watch
        and not ran_here
        and not is_inside_ai_tool()
        and not explicit_name_provided
    ):
        print("\nOpening hcom UI...")
        time.sleep(2)

        from ...ui import run_tui

        return run_tui(hcom_path())
    else:
        from ...core.tips import print_launch_tips
        from ...core.instances import load_instance_position

        print_launch_tips(
            launched=launched,
            tag=get_config().tag or None,
            instance_names=instance_names,
            launcher_name=launcher_name,
            launcher_participating=load_instance_position(launcher_name) is not None if launcher_name else False,
            background=False,
        )

    return 0 if failed == 0 else 1


def cmd_launch_codex(
    argv: list[str],
    *,
    launcher_name: str | None = None,
    ctx: "CommandContext | None" = None,
) -> int:
    """Launch Codex instances: hcom <N> codex [codex-args...]

    Args:
        argv: Command line arguments (identity flags already stripped)
        launcher_name: Explicit launcher identity from --name flag (CLI layer parsed this)
        ctx: Command context with explicit_name if --name was provided

    Examples:
        hcom 1 codex                          # Launch 1 Codex instance (interactive)
        hcom 2 codex                          # Launch 2 Codex instances
        hcom 1 codex resume <id>              # Resume specific thread (interactive)

    Note: 'codex resume' without explicit thread-id (interactive picker) is not supported.
    Note: 'codex resume --last' is not supported.

    Raises:
        HcomError: On hook setup failure or launch failure.
    """
    # Platform check - Codex PTY requires Unix-only APIs
    if IS_WINDOWS:
        raise CLIError(
            "Codex CLI integration requires PTY (pseudo-terminal) which is not available on Windows.\n"
            "Use 'hcom N claude' for Claude Code on Windows (hooks-based, no PTY required)."
        )

    from ...launcher import launch as unified_launch
    from ...tools.codex.args import resolve_codex_args
    from ...core.config import get_config

    # Hook setup moved to launcher.launch() - single source of truth

    # Parse count (required first arg)
    if not argv or not argv[0].isdigit():
        raise CLIError("Usage: hcom <N> codex [codex-args...]")

    count = int(argv[0])
    if count <= 0:
        raise CLIError("Count must be positive.")
    if count > 10:
        raise CLIError("Too many Codex agents (max 10).")
    argv = argv[1:]

    # Skip 'codex' keyword
    if argv and argv[0] == "codex":
        argv = argv[1:]

    # Note: Identity flags (--name) already stripped by CLI layer

    # Check for --no-auto-watch flag (used by TUI to prevent opening another watch window)
    no_auto_watch = "--no-auto-watch" in argv
    if no_auto_watch:
        argv = [arg for arg in argv if arg != "--no-auto-watch"]

    # Parse using proper Codex args parser - merge env (HCOM_CODEX_ARGS) and CLI args
    from ...tools.codex.args import merge_codex_args

    env_spec = resolve_codex_args(None, get_config().codex_args)
    cli_spec = resolve_codex_args(argv, None)
    spec = (
        merge_codex_args(env_spec, cli_spec)
        if (cli_spec.clean_tokens or cli_spec.positional_tokens or cli_spec.subcommand)
        else env_spec
    )

    # Validate parsed args (strict by default, overridable).
    from ...shared import skip_tool_args_validation, HCOM_SKIP_TOOL_ARGS_VALIDATION_ENV

    if spec.has_errors() and not skip_tool_args_validation():
        raise CLIError(
            "\n".join(
                [
                    *spec.errors,
                    f"Tip: set {HCOM_SKIP_TOOL_ARGS_VALIDATION_ENV}=1 to bypass hcom validation and let codex handle args.",
                ]
            )
        )

    resume_thread_id = None

    # Launch confirmation gate: inside AI tools, require HCOM_GO=1
    # Show preview if: has args OR count > 5
    has_args = argv and len(argv) > 0
    if is_inside_ai_tool() and not get_hcom_go() and (has_args or count > 5):
        _print_launch_preview("codex", count, False, argv)  # Codex always interactive
        return 0

    # Handle resume/fork subcommand
    if spec.subcommand in ("resume", "fork"):
        if not spec.positional_tokens:
            raise CLIError(f"'codex {spec.subcommand}' requires explicit thread-id (interactive picker not supported)")
        if spec.has_flag(["--last"]):
            raise CLIError(f"'codex {spec.subcommand} --last' not supported - use explicit thread-id")
        if spec.subcommand == "resume":
            resume_thread_id = spec.positional_tokens[0]

    # Exec mode (headless) not supported for Codex in hcom
    if spec.is_exec:
        raise CLIError("'codex exec' is not supported. Use interactive codex or headless claude.")

    # Prevent identity collision: resume targets one specific thread
    if resume_thread_id and count > 1:
        raise CLIError(f"Cannot resume the same thread-id with multiple agents (count={count})")

    # Build final args list (include subcommand for resume/fork/review)
    include_subcommand = spec.subcommand in ("resume", "fork", "review")
    codex_args = spec.rebuild_tokens(include_subcommand=include_subcommand)

    # Determine if instance will run in current terminal (blocking mode)
    from ...launcher import will_run_in_current_terminal

    ran_here = will_run_in_current_terminal(count, False)  # Codex always interactive

    # Resolve launcher identity: use explicit --name if provided, else auto-resolve
    if not launcher_name:
        try:
            launcher_name = resolve_identity().name
        except Exception:
            launcher_name = None  # Let unified_launch handle fallback

    result = unified_launch(
        "codex",
        count,
        codex_args,
        launcher=launcher_name,
        background=False,  # Codex headless not supported
        system_prompt=get_config().codex_system_prompt or None,
    )

    # Surface per-instance launch errors
    for err in result.get("errors", []):
        error_msg = err.get("error", "Unknown error")
        print(f"Error: {error_msg}", file=sys.stderr)

    launched = result["launched"]
    failed = result["failed"]

    if launched == 0 and failed > 0:
        return 1  # All failed, exit with error

    instance_names: list[str] = []
    for h in result.get("handles", []):
        name = h.get("instance_name")
        if name:
            instance_names.append(name)
            print(f"Started the launch process for Codex: {name}")

    print(f"\nStarted the launch process for {launched} Codex agent{'s' if launched != 1 else ''}")
    print(f"Batch id: {result['batch_id']}")
    print("To block until ready, fail, or timeout (30s), run: hcom events launch")

    # Auto-launch TUI if:
    # - Not print mode, not auto-watch disabled, all launched, interactive terminal
    # - Did NOT run in current terminal (ran_here=True means single instance already finished)
    # - NOT inside AI tool (would hijack the session)
    # - NOT ad-hoc launch with --name (external script doesn't want TUI)
    terminal_mode = get_config().terminal
    explicit_name_provided = ctx and ctx.explicit_name
    if (
        terminal_mode != "print"
        and failed == 0
        and is_interactive()
        and not no_auto_watch
        and not ran_here
        and not is_inside_ai_tool()
        and not explicit_name_provided
    ):
        if instance_names:
            print(f"\n  • Send message: hcom send '@{instance_names[0]} hello'")
        print("\nOpening hcom UI...")
        time.sleep(2)

        from ...ui import run_tui

        return run_tui(hcom_path())
    else:
        from ...core.tips import print_launch_tips
        from ...core.instances import load_instance_position

        print_launch_tips(
            launched=launched,
            tag=get_config().tag or None,
            instance_names=instance_names,
            launcher_name=launcher_name,
            launcher_participating=load_instance_position(launcher_name) is not None if launcher_name else False,
            background=False,
        )

    return 0 if failed == 0 else 1
