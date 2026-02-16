"""Launch AI tool instances (Claude, Gemini, Codex)."""

import os
import sys
import time

from ..utils import (
    CLIError,
    is_interactive,
    resolve_identity,
)
from ...shared import (
    FG_YELLOW,
    RESET,
    IS_WINDOWS,
    is_inside_ai_tool,
    CommandContext,
    skip_tool_args_validation,
    HCOM_SKIP_TOOL_ARGS_VALIDATION_ENV,
)
from ...core.thread_context import get_hcom_go, get_cwd
from ...core.config import get_config
from ...core.paths import hcom_path
from ...core.instances import (
    load_instance_position,
)
from ...core.tool_utils import build_hcom_command


def _verify_hooks_for_tool(tool: str) -> bool:
    """Verify if hooks are installed for the specified tool.

    Returns True if hooks are installed and verified, False otherwise.
    """
    try:
        if tool == "claude":
            from ...tools.claude.settings import verify_claude_hooks_installed

            return verify_claude_hooks_installed(check_permissions=False)
        elif tool == "gemini":
            from ...tools.gemini.settings import verify_gemini_hooks_installed

            return verify_gemini_hooks_installed(check_permissions=False)
        elif tool == "codex":
            from ...tools.codex.settings import verify_codex_hooks_installed

            return verify_codex_hooks_installed(check_permissions=False)
        else:
            return True  # Unknown tool - don't block
    except Exception:
        return True  # On error, don't block (optimistic)


def _print_launch_preview(tool: str, count: int, background: bool, args: list[str] | None = None) -> None:
    """Launch documentation for AI. Bootstrap has no launch info - this is it."""
    from ...core.runtime import build_claude_env
    from ...core.config import KNOWN_CONFIG_KEYS

    config = get_config()
    hcom_cmd = build_hcom_command()

    # Active env
    active_env = build_claude_env()
    for k in KNOWN_CONFIG_KEYS:
        if k in os.environ:
            active_env[k] = os.environ[k]

    def fmt(k):
        return active_env.get(k, "")

    # Tool-specific args
    args_key = f"HCOM_{tool.upper()}_ARGS"
    env_args = active_env.get(args_key, "")
    cli_args = " ".join(args) if args else ""

    # Tool-specific CLI help
    if tool == "claude":
        cli_help = (
            "positional | -p 'prompt' (headless) | --model opus|sonnet|haiku | --agent <name-from-./claude/agents/> | "
            "--system-prompt | --resume <id> | --dangerously-skip-permissions"
        )
        mode_note = (
            "\n  -p allows hcom + readonly permissions by default, to add: --tools Bash,Write,Edit,etc"
            if background
            else ""
        )
    elif tool == "gemini":
        cli_help = (
            "-i 'prompt' (required for initial prompt) | --model | --yolo | --resume | (system prompt via env var)"
        )
        mode_note = (
            "\n  Note: Gemini headless not supported in hcom, use claude headless or gemini interactive"
            if background
            else ""
        )
    elif tool == "codex":
        cli_help = (
            "'prompt' (positional) | --model | --sandbox (read-only|workspace-write|danger-full-access) "
            "| resume (subcommand) | -i 'image' | (system prompt via env var)"
        )
        mode_note = (
            "\n  Note: Codex headless not supported in hcom, use claude headless or codex interactive"
            if background
            else ""
        )
    else:
        cli_help = f"see `{tool} --help`"
        mode_note = ""

    # Tool-specific env vars shown in preview
    timeout_str = f"{config.timeout}s"
    subagent_timeout_str = f"{config.subagent_timeout}s"
    if tool == "claude":
        # HCOM_TIMEOUT only applies to headless/vanilla, not interactive PTY
        if background:
            tool_env_vars = f"HCOM_TIMEOUT={timeout_str}\n    HCOM_SUBAGENT_TIMEOUT={subagent_timeout_str}"
        else:
            tool_env_vars = f"HCOM_SUBAGENT_TIMEOUT={subagent_timeout_str}"
    elif tool == "gemini":
        tool_env_vars = f"HCOM_GEMINI_SYSTEM_PROMPT={config.gemini_system_prompt}"
    elif tool == "codex":
        tool_env_vars = f"HCOM_CODEX_SYSTEM_PROMPT={config.codex_system_prompt}"
    else:
        tool_env_vars = ""

    print(f"""
== LAUNCH PREVIEW ==
This shows launch config and info.
Set HCOM_GO=1 and run again to proceed.

Tool: {tool}  Count: {count}  Mode: {"headless" if background else "interactive"}{mode_note}
Directory: {get_cwd()}

Config (override: VAR=val {hcom_cmd} ...):
  HCOM_TAG={fmt("HCOM_TAG")}
  HCOM_TERMINAL={fmt("HCOM_TERMINAL") or "default"}
  HCOM_HINTS={fmt("HCOM_HINTS") or "(none)"}
  HCOM_NOTES={fmt("HCOM_NOTES") or "(none)"}
  {tool_env_vars}

Args:
  From env ({args_key}): {env_args or "(none)"}
  From CLI: {cli_args or "(none)"}
  (CLI overrides env per-flag)

CLI (see `{tool} --help`):
  {cli_help}

Launch Behavior:
  - Agents auto-register with hcom & get session info on startup
  - Interactive instances open in new terminal windows
  - Headless agents run in background, log to ~/.hcom/.tmp/logs/
  - Use HCOM_TAG to group instances: HCOM_TAG=team {hcom_cmd} 3
  - Use `hcom events launch` to block until agents are ready or launch failed

Initial Prompt Tip:
  Tell instances to use 'hcom' in the initial prompt to guarantee
  they respond correctly. Define explicit roles/tasks.
""")


def cmd_launch_tool(
    tool: str,
    argv: list[str],
    *,
    launcher_name: str | None = None,
    ctx: "CommandContext | None" = None,
) -> int:
    """Launch AI tool instances: hcom [N] [claude|gemini|codex] [args]

    Unified entry point for all tool launch commands (CLI path).

    Args:
        tool: Tool name ("claude", "gemini", "codex")
        argv: Command line arguments (identity flags already stripped)
        launcher_name: Explicit launcher identity from --name flag
        ctx: Command context with explicit_name if --name was provided

    Raises:
        CLIError: On argument validation failure.
        HcomError: On hook setup failure or launch failure.
    """
    from ...launcher import launch as unified_launch, will_run_in_current_terminal

    config = get_config()

    # --- Platform check (gemini/codex require PTY = Unix only) ---
    if tool in ("gemini", "codex") and IS_WINDOWS:
        tool_name = "Gemini" if tool == "gemini" else "Codex"
        raise CLIError(
            f"{tool_name} CLI integration requires PTY (pseudo-terminal) which is not available on Windows.\n"
            "Use 'hcom N claude' for Claude Code on Windows (hooks-based, no PTY required)."
        )

    # --- Parse count and skip tool keyword ---
    count = 1
    if argv and argv[0].isdigit():
        count = int(argv[0])
        if count <= 0:
            raise CLIError("Count must be positive.")
        max_count = 100 if tool == "claude" else 10
        if count > max_count:
            raise CLIError(f"Too many agents requested (max {max_count}).")
        argv = argv[1:]

    if argv and argv[0] == tool:
        argv = argv[1:]

    # --- Extract --no-auto-watch flag ---
    no_auto_watch = "--no-auto-watch" in argv
    if no_auto_watch:
        argv = [arg for arg in argv if arg != "--no-auto-watch"]

    forwarded = argv

    # --- Tool-specific arg parsing (env + CLI merge) ---
    background = False
    tool_args: list[str] = []
    system_prompt: str | None = None

    match tool:
        case "claude":
            from ...tools.claude.args import (
                resolve_claude_args,
                merge_claude_args,
                add_background_defaults,
                validate_conflicts,
            )

            env_spec = resolve_claude_args(None, config.claude_args)
            cli_spec = resolve_claude_args(forwarded or None, None)

            if cli_spec.clean_tokens or cli_spec.positional_tokens:
                spec = merge_claude_args(env_spec, cli_spec)
            else:
                spec = env_spec

            if spec.has_errors() and not skip_tool_args_validation():
                raise CLIError(
                    "\n".join([
                        *spec.errors,
                        f"Tip: set {HCOM_SKIP_TOOL_ARGS_VALIDATION_ENV}=1 to bypass hcom validation and let claude handle args.",
                    ])
                )

            # Warnings (claude-only)
            for warning in validate_conflicts(spec):
                print(f"{FG_YELLOW}Warning:{RESET} {warning}", file=sys.stderr)

            spec = add_background_defaults(spec)
            background = spec.is_background
            tool_args = spec.rebuild_tokens()

        case "gemini":
            from ...tools.gemini.args import resolve_gemini_args, merge_gemini_args

            g_env = resolve_gemini_args(None, config.gemini_args)
            g_cli = resolve_gemini_args(forwarded or None, None)
            g_spec = merge_gemini_args(g_env, g_cli) if (g_cli.clean_tokens or g_cli.positional_tokens) else g_env

            if g_spec.has_errors() and not skip_tool_args_validation():
                raise CLIError(
                    "\n".join([
                        *g_spec.errors,
                        f"Tip: set {HCOM_SKIP_TOOL_ARGS_VALIDATION_ENV}=1 to bypass hcom validation and let gemini handle args.",
                    ])
                )

            # Reject headless mode
            if g_spec.positional_tokens or g_spec.has_flag(["-p", "--prompt"], ("-p=", "--prompt=")):
                headless_type = "positional query" if g_spec.positional_tokens else "-p/--prompt flag"
                raise CLIError(
                    f"Gemini headless mode not supported in hcom (attempted: {headless_type}).\n"
                    "  • For interactive: hcom N gemini\n"
                    '  • For interactive with initial prompt: hcom N gemini -i "prompt"\n'
                    '  • For headless: hcom N claude -p "task"'
                )

            tool_args = g_spec.rebuild_tokens()
            system_prompt = config.gemini_system_prompt or None

        case "codex":
            from ...tools.codex.args import resolve_codex_args, merge_codex_args

            x_env = resolve_codex_args(None, config.codex_args)
            x_cli = resolve_codex_args(forwarded or None, None)
            x_spec = (
                merge_codex_args(x_env, x_cli)
                if (x_cli.clean_tokens or x_cli.positional_tokens or x_cli.subcommand)
                else x_env
            )

            if x_spec.has_errors() and not skip_tool_args_validation():
                raise CLIError(
                    "\n".join([
                        *x_spec.errors,
                        f"Tip: set {HCOM_SKIP_TOOL_ARGS_VALIDATION_ENV}=1 to bypass hcom validation and let codex handle args.",
                    ])
                )

            # Reject exec mode
            if x_spec.is_exec:
                raise CLIError("'codex exec' is not supported. Use interactive codex or headless claude.")

            include_subcommand = x_spec.subcommand in ("resume", "fork", "review")
            tool_args = x_spec.rebuild_tokens(include_subcommand=include_subcommand)
            system_prompt = config.codex_system_prompt or None

        case _:
            raise CLIError(f"Unknown tool: {tool}")

    # --- HCOM_GO confirmation gate ---
    if is_inside_ai_tool() and not get_hcom_go() and (forwarded or count > 5):
        _print_launch_preview(tool, count, background, forwarded)
        return 0

    # --- Resolve launcher identity ---
    if launcher_name:
        launcher = launcher_name
    else:
        try:
            launcher = resolve_identity().name
        except Exception:
            launcher = "user"
    launcher_data = load_instance_position(launcher)
    launcher_participating = launcher_data is not None

    # --- PTY and terminal decisions ---
    use_pty = tool != "claude" or (not background and not IS_WINDOWS)
    ran_here = will_run_in_current_terminal(count, background)
    tag = config.tag

    # --- Launch ---
    result = unified_launch(
        tool,
        count,
        tool_args,
        tag=tag,
        background=background,
        cwd=str(get_cwd()),
        launcher=launcher,
        pty=use_pty,
        system_prompt=system_prompt,
        skip_validation=True,
    )

    # --- Surface errors ---
    for err in result.get("errors", []):
        print(f"Error: {err.get('error', 'Unknown error')}", file=sys.stderr)

    launched = result["launched"]
    failed = result["failed"]
    batch_id = result["batch_id"]
    instance_names = [h["instance_name"] for h in result.get("handles", []) if h.get("instance_name")]

    # Print background log files (claude headless)
    for log_file in result.get("log_files", []):
        print(f"Headless launched, log: {log_file}")

    if launched == 0 and failed > 0:
        return 1

    # --- Print summary ---
    tool_label = tool.capitalize()
    if failed > 0:
        print(f"Started the launch process for {launched}/{count} {tool_label} agent{'s' if count != 1 else ''} ({failed} failed)")
    else:
        print(f"Started the launch process for {launched} {tool_label} agent{'s' if launched != 1 else ''}")
    if instance_names:
        print(f"Names: {', '.join(instance_names)}")
    print(f"Batch id: {batch_id}")
    print("To block until ready or fail (30s timeout), run: hcom events launch")

    # --- Auto-TUI or tips ---
    terminal_mode = config.terminal
    explicit_name_provided = ctx and ctx.explicit_name

    if (
        terminal_mode != "print"
        and failed == 0
        and is_interactive()
        and not background
        and not no_auto_watch
        and not ran_here
        and not is_inside_ai_tool()
        and not explicit_name_provided
    ):
        if tag:
            print(f"\n  • Send to {tag} team: hcom send '@{tag}- message'")

        print("\nOpening hcom UI...")
        time.sleep(2)

        from ...ui import run_tui

        return run_tui(hcom_path())
    else:
        from ...core.tips import print_launch_tips

        print_launch_tips(
            launched=launched,
            tag=tag,
            launcher_name=launcher if launcher != "user" else None,
            launcher_participating=launcher_participating,
            background=background,
        )

    return 0 if failed == 0 else 1


def cmd_launch(
    argv: list[str],
    *,
    launcher_name: str | None = None,
    ctx: "CommandContext | None" = None,
) -> int:
    """Launch Claude instances. Alias for cmd_launch_tool("claude", ...)."""
    return cmd_launch_tool("claude", argv, launcher_name=launcher_name, ctx=ctx)
