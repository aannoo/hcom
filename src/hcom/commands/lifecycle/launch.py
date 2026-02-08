"""Launch Claude instances."""

import os
import sys
import time

from ..utils import (
    CLIError,
    format_error,
    is_interactive,
    resolve_identity,
)
from ...shared import (
    FG_YELLOW,
    RESET,
    IS_WINDOWS,
    is_inside_ai_tool,
    CommandContext,
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
        v = active_env.get(k, "")
        return v if v else ""

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

    # Format timeout nicely
    timeout = config.timeout
    timeout_str = f"{timeout}s"
    subagent_timeout = config.subagent_timeout
    subagent_timeout_str = f"{subagent_timeout}s"
    claude_env_vars = ""
    if tool == "claude":
        # HCOM_TIMEOUT only applies to headless/vanilla, not interactive PTY
        if background:
            claude_env_vars = f"""HCOM_TIMEOUT={timeout_str}
    HCOM_SUBAGENT_TIMEOUT={subagent_timeout_str}"""
        else:
            claude_env_vars = f"""HCOM_SUBAGENT_TIMEOUT={subagent_timeout_str}"""
    gemini_env_vars = ""
    if tool == "gemini":
        gemini_env_vars = f"""HCOM_GEMINI_SYSTEM_PROMPT={config.gemini_system_prompt}"""
    codex_env_vars = ""
    if tool == "codex":
        codex_env_vars = f"""HCOM_CODEX_SYSTEM_PROMPT={config.codex_system_prompt}"""

    from ...core.thread_context import get_cwd

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
  {claude_env_vars}{gemini_env_vars}{codex_env_vars}

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


def cmd_launch(
    argv: list[str],
    *,
    launcher_name: str | None = None,
    ctx: "CommandContext | None" = None,
) -> int:
    """Launch Claude instances: hcom [N] [claude] [args]

    Args:
        argv: Command line arguments (identity flags already stripped)
        launcher_name: Explicit launcher identity from --name flag (CLI layer parsed this)
        ctx: Command context with explicit_name if --name was provided

    Raises:
        HcomError: On hook setup failure or launch failure.
    """
    from ...core.ops import op_launch
    from ...shared import (
        HcomError,
        skip_tool_args_validation,
        HCOM_SKIP_TOOL_ARGS_VALIDATION_ENV,
    )

    # Hook setup moved to launcher.launch() - single source of truth

    try:
        # Parse arguments: hcom [N] [claude] [args]
        # Note: Identity flags (--name) already stripped by CLI layer
        count = 1

        # Extract count if first arg is digit
        if argv and argv[0].isdigit():
            count = int(argv[0])
            if count <= 0:
                raise CLIError("Count must be positive.")
            if count > 100:
                raise CLIError("Too many agents requested (max 100).")
            argv = argv[1:]

        # Skip 'claude' keyword if present
        if argv and argv[0] == "claude":
            argv = argv[1:]

        # Forward all remaining args to claude CLI
        forwarded = argv

        # Check for --no-auto-watch flag (used by TUI to prevent opening another watch window)
        no_auto_watch = "--no-auto-watch" in forwarded
        if no_auto_watch:
            forwarded = [arg for arg in forwarded if arg != "--no-auto-watch"]

        # Get tag from config
        tag = get_config().tag

        # Lazy import to avoid ~3ms overhead on CLI startup
        from ...tools.claude.args import (
            resolve_claude_args,
            merge_claude_args,
            add_background_defaults,
            validate_conflicts,
        )

        # Phase 1: Parse and merge Claude args (env + CLI with CLI precedence)
        env_spec = resolve_claude_args(None, get_config().claude_args)
        cli_spec = resolve_claude_args(forwarded if forwarded else None, None)

        # Merge: CLI overrides env on per-flag basis, inherits env if CLI has no args
        if cli_spec.clean_tokens or cli_spec.positional_tokens:
            spec = merge_claude_args(env_spec, cli_spec)
        else:
            spec = env_spec

        # Validate parsed args
        if spec.has_errors() and not skip_tool_args_validation():
            raise CLIError(
                "\n".join(
                    [
                        *spec.errors,
                        f"Tip: set {HCOM_SKIP_TOOL_ARGS_VALIDATION_ENV}=1 to bypass hcom validation and let claude handle args.",
                    ]
                )
            )

        # Check for conflicts (warnings only, not errors)
        warnings = validate_conflicts(spec)
        for warning in warnings:
            print(f"{FG_YELLOW}Warning:{RESET} {warning}", file=sys.stderr)

        # Add HCOM background mode enhancements
        spec = add_background_defaults(spec)

        # Extract values from spec
        background = spec.is_background

        # Launch confirmation gate: inside AI tools, require HCOM_GO=1
        # Show preview if: has args OR count > 5
        has_args = forwarded and len(forwarded) > 0
        if is_inside_ai_tool() and not get_hcom_go() and (has_args or count > 5):
            _print_launch_preview("claude", count, background, forwarded)
            return 0
        claude_args = spec.rebuild_tokens()

        # Resolve launcher identity: use explicit --name if provided, else auto-resolve
        if launcher_name:
            launcher = launcher_name
        else:
            try:
                launcher = resolve_identity().name
            except HcomError:
                launcher = "user"
        launcher_data = load_instance_position(launcher)
        launcher_participating = launcher_data is not None  # Row exists = participating

        # PTY mode: use PTY wrapper for interactive Claude (not headless, not Windows)
        use_pty = not background and not IS_WINDOWS

        # Determine if instance will run in current terminal (blocking mode)
        from ...launcher import will_run_in_current_terminal

        ran_here = will_run_in_current_terminal(count, background)

        # Call op_launch
        result = op_launch(
            count,
            claude_args,
            launcher=launcher,
            tag=tag,
            background=background,
            cwd=str(get_cwd()),
            pty=use_pty,
        )

        launched = result["launched"]
        failed = result["failed"]
        batch_id = result["batch_id"]
        instance_names = [h["instance_name"] for h in result.get("handles", []) if h.get("instance_name")]

        # Print background log files
        for log_file in result.get("log_files", []):
            print(f"Headless launched, log: {log_file}")

        # Show results
        if failed > 0:
            print(
                f"Started the launch process for {launched}/{count} Claude agent{'s' if count != 1 else ''} ({failed} failed)"
            )
        else:
            print(f"Started the launch process for {launched} Claude agent{'s' if launched != 1 else ''}")

        print(f"Batch id: {batch_id}")
        print("To block until ready or fail (30s timeout), run: hcom events launch")

        # Auto-launch TUI if:
        # - Not print mode, not background, not auto-watch disabled, all launched, interactive terminal
        # - Did NOT run in current terminal (ran_here=True means single instance already finished)
        # - NOT inside AI tool (would hijack the session)
        # - NOT ad-hoc launch with --name (external script doesn't want TUI)
        terminal_mode = get_config().terminal
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
                instance_names=instance_names,
                launcher_name=launcher if launcher != "user" else None,
                launcher_participating=launcher_participating,
                background=background,
            )

            return 0

    except (CLIError, HcomError) as e:
        print(format_error(str(e)), file=sys.stderr)
        return 1
    except Exception as e:
        print(format_error(str(e)), file=sys.stderr)
        return 1
