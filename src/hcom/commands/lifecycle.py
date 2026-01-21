"""Lifecycle commands for HCOM instances"""

import os
import sys
import time
from pathlib import Path
from .utils import (
    CLIError,
    format_error,
    is_interactive,
    resolve_identity,
    validate_flags,
)
from ..shared import (
    BOLD,
    FG_YELLOW,
    RESET,
    IS_WINDOWS,
    HcomError,
    is_inside_ai_tool,
    detect_vanilla_tool,
    detect_current_tool,
    CommandContext,
)

# Note: tools.claude.args imports are deferred to cmd_launch_claude() to avoid ~3ms import overhead
from ..core.config import get_config
from ..core.paths import hcom_path
from ..core.instances import (
    load_instance_position,
    update_instance_position,
    is_subagent_instance,
    SKIP_HISTORY,
    parse_running_tasks,
)
from ..hooks.subagent import in_subagent_context
from ..core.db import iter_instances
from ..core.tool_utils import stop_instance, build_hcom_command


def _verify_hooks_for_tool(tool: str) -> bool:
    """Verify if hooks are installed for the specified tool.

    Returns True if hooks are installed and verified, False otherwise.
    """
    try:
        if tool == "claude":
            from ..hooks.settings import verify_claude_hooks_installed

            return verify_claude_hooks_installed(check_permissions=False)
        elif tool == "gemini":
            from ..tools.gemini.settings import verify_gemini_hooks_installed

            return verify_gemini_hooks_installed(check_permissions=False)
        elif tool == "codex":
            from ..tools.codex.settings import verify_codex_hooks_installed

            return verify_codex_hooks_installed(check_permissions=False)
        else:
            return True  # Unknown tool - don't block
    except Exception:
        return True  # On error, don't block (optimistic)


def _print_launch_preview(tool: str, count: int, background: bool, args: list[str] | None = None) -> None:
    """Launch documentation for AI. Bootstrap has no launch info - this is it."""
    from ..core.runtime import build_claude_env
    from ..core.config import KNOWN_CONFIG_KEYS

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
        cli_help = "-p (headless) | --model opus|sonnet|haiku | --agent <name-from-./claude/agents/> |\n--system-prompt | --resume <id> | --dangerously-skip-permissions"
        mode_note = (
            "-p allows hcom + readonly permissions by default, to add: --tools Bash,Write,Edit,etc"
            if background
            else ""
        )
    elif tool == "gemini":
        cli_help = "-i 'prompt' | --model | --yolo | --resume <id> | (system prompt via env var)"
        mode_note = (
            "\n  Note: Gemini headless not supported in hcom, use claude headless or gemini interactive"
            if background
            else ""
        )
    elif tool == "codex":
        cli_help = " 'prompt' | --model | --sandbox danger-full-access | resume <id> | (system prompt via env var) | -i 'image'"
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

    print(f"""
== LAUNCH PREVIEW ==
This shows launch config and info.
Set HCOM_GO=1 and run again to proceed.

Tool: {tool}  Count: {count}  Mode: {"headless" if background else "interactive"}{mode_note}
Directory: {os.getcwd()}

Config (override: VAR=val {hcom_cmd} ...):
  HCOM_TAG={fmt("HCOM_TAG")}
  HCOM_TERMINAL={fmt("HCOM_TERMINAL") or "default"}
  HCOM_HINTS={fmt("HCOM_HINTS") or "(none)"}
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
    from ..core.ops import op_launch
    from ..shared import (
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
        from ..tools.claude.args import (
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
        if is_inside_ai_tool() and os.environ.get("HCOM_GO") != "1":
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
        from ..launcher import will_run_in_current_terminal

        ran_here = will_run_in_current_terminal(count, background)

        # Call op_launch
        result = op_launch(
            count,
            claude_args,
            launcher=launcher,
            tag=tag,
            background=background,
            cwd=os.getcwd(),
            pty=use_pty,
        )

        launched = result["launched"]
        failed = result["failed"]
        batch_id = result["batch_id"]

        # Print background log files
        for log_file in result.get("log_files", []):
            print(f"Headless launched, log: {log_file}")

        # Show results
        if failed > 0:
            print(f"Started the launch process for {launched}/{count} Claude agent{'s' if count != 1 else ''} ({failed} failed)")
        else:
            print(f"Started the launch process for {launched} Claude agent{'s' if launched != 1 else ''}")

        print(f"Batch id: {batch_id}")
        print("To block until ready or fail, run: hcom events launch")

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

            from ..ui import run_tui

            return run_tui(hcom_path())
        else:
            tips = []
            if tag:
                tips.append(f"Send to {tag} team: hcom send '@{tag}- message'")

            if launched > 0:
                if is_inside_ai_tool():
                    if launcher_participating:
                        tips.append(
                            f"You'll be automatically notified when all {launched} instances are launched & ready"
                        )
                    else:
                        tips.append("Run 'hcom start' to receive automatic notifications/messages from instances")
                else:
                    tips.append("Check status: hcom list")

            if tips:
                print("\n" + "\n".join(f"  • {tip}" for tip in tips) + "\n")

            return 0

    except (CLIError, HcomError) as e:
        print(format_error(str(e)), file=sys.stderr)
        return 1
    except Exception as e:
        print(format_error(str(e)), file=sys.stderr)
        return 1


def _print_stop_preview(target: str, instances: list[dict], target_names: list[str] | None = None) -> None:
    """Print stop preview for AI tools. Shows what will be stopped.

    Args:
        target: 'all', 'tag:name', or 'multi' for multiple explicit targets
        instances: List of instance dicts to stop
        target_names: For 'multi' mode, the original target names from CLI
    """
    from ..core.tool_utils import build_hcom_command

    hcom_cmd = build_hcom_command()
    count = len(instances)
    names = [i["name"] for i in instances]

    # Categorize instances
    headless = [i for i in instances if i.get("background")]
    interactive = [i for i in instances if not i.get("background")]

    # Build instance list display
    if count <= 8:
        instance_list = ", ".join(names)
    else:
        instance_list = ", ".join(names[:6]) + f" ... (+{count - 6} more)"

    if target == "all":
        print(f"""
== STOP ALL PREVIEW ==
This will stop all {count} local instance{"s" if count != 1 else ""}.

Instances to stop:
  {instance_list}

What happens:
  • Headless instances ({len(headless)}): process killed (SIGTERM, then SIGKILL after 2s)
  • Interactive instances ({len(interactive)}): notified via TCP (graceful)
  • All: stopped event logged with snapshot, instance rows deleted
  • Subagents: recursively stopped when parent stops

Instance data preserved in events table (life.stopped with snapshot).

Set HCOM_GO=1 and run again to proceed:
  HCOM_GO=1 {hcom_cmd} stop all
""")
    elif target == "multi":
        # Multiple explicit targets
        cmd_targets = " ".join(target_names or names)
        print(f"""
== STOP PREVIEW ==
This will stop {count} instance{"s" if count != 1 else ""}.

Instances to stop:
  {instance_list}

What happens:
  • Headless instances ({len(headless)}): process killed (SIGTERM, then SIGKILL after 2s)
  • Interactive instances ({len(interactive)}): notified via TCP (graceful)
  • All: stopped event logged with snapshot, instance rows deleted
  • Subagents: recursively stopped when parent stops

Instance data preserved in events table (life.stopped with snapshot).

Set HCOM_GO=1 and run again to proceed:
  HCOM_GO=1 {hcom_cmd} stop {cmd_targets}
""")
    elif target.startswith("tag:"):
        # tag:name target
        tag = target[4:]  # Remove tag: prefix
        print(f"""
== STOP tag:{tag} PREVIEW ==
This will stop all {count} instance{"s" if count != 1 else ""} with tag '{tag}'.

Instances to stop:
  {instance_list}

What happens:
  • Headless instances ({len(headless)}): process killed (SIGTERM, then SIGKILL after 2s)
  • Interactive instances ({len(interactive)}): notified via TCP (graceful)
  • All: stopped event logged with snapshot, instance rows deleted
  • Subagents: recursively stopped when parent stops

Instance data preserved in events table (life.stopped with snapshot).

Set HCOM_GO=1 and run again to proceed:
  HCOM_GO=1 {hcom_cmd} stop tag:{tag}
""")


def cmd_stop(argv: list[str], *, ctx: CommandContext | None = None) -> int:
    """End hcom participation (deletes instance).

    Usage: hcom stop [name...] | hcom stop all | hcom stop tag:<name>

    Examples:
        hcom stop              # Stop self (inside Claude/Gemini/Codex)
        hcom stop nova         # Stop single instance
        hcom stop nova piko    # Stop multiple instances
        hcom stop all          # Stop all local instances
        hcom stop tag:team     # Stop all instances with tag 'team'

    Note: Stop permanently ends participation by deleting the instance row.
    A new identity is created on next start.
    """
    from ..shared import is_inside_ai_tool

    # Validate flags
    if error := validate_flags("stop", argv):
        print(format_error(error), file=sys.stderr)
        return 1

    # Identity (sender): CLI supplies ctx (preferred). Direct calls may still pass --name.
    explicit_initiator = ctx.explicit_name if ctx else None
    if ctx is None:
        from .utils import parse_name_flag

        explicit_initiator, argv = parse_name_flag(argv)

    # Remove flags to get targets (multiple allowed)
    targets = [a for a in argv if not a.startswith("--")]

    # Handle 'all' target (must be sole target)
    if "all" in targets:
        if len(targets) > 1:
            raise CLIError("'all' cannot be combined with other targets")
        # Only stop local instances (not remote ones from other devices)
        instances = [i for i in iter_instances() if not i.get("origin_device_id")]

        if not instances:
            print("Nothing to stop")
            return 0

        # Confirmation gate: inside AI tools, require HCOM_GO=1
        if is_inside_ai_tool() and os.environ.get("HCOM_GO") != "1":
            _print_stop_preview("all", instances)
            return 0

        stopped_count = 0
        bg_logs = []
        stopped_names = []
        # Initiator name for event logging
        if ctx and ctx.identity and ctx.identity.kind == "instance":
            launcher = ctx.identity.name
        elif explicit_initiator:
            launcher = explicit_initiator
        else:
            try:
                launcher = resolve_identity().name
            except HcomError:
                launcher = "cli"
        for instance_data in instances:
            # Row exists = participating (stop all instances)
            instance_name = instance_data["name"]
            stop_instance(instance_name, initiated_by=launcher, reason="stop_all")
            stopped_names.append(instance_name)
            stopped_count += 1

            # Track background logs
            if instance_data.get("background"):
                log_file = instance_data.get("background_log_file", "")
                if log_file:
                    bg_logs.append((instance_name, log_file))

        if stopped_count == 0:
            print("Nothing to stop")
        else:
            print(f"Stopped: {', '.join(stopped_names)}")

            # Show background logs if any
            if bg_logs:
                print()
                print("Headless logs:")
                for name, log_file in bg_logs:
                    print(f"  {name}: {log_file}")

        return 0

    # Handle tag:name syntax - stop all instances with matching tag
    if len(targets) == 1 and targets[0].startswith("tag:"):
        tag = targets[0][4:]
        tag_matches = [i for i in iter_instances() if i.get("tag") == tag and not i.get("origin_device_id")]
        if not tag_matches:
            raise CLIError(f"No instances with tag '{tag}'")

        # Confirmation gate: inside AI tools, require HCOM_GO=1
        if is_inside_ai_tool() and os.environ.get("HCOM_GO") != "1":
            _print_stop_preview(f"tag:{tag}", tag_matches)
            return 0

        # Resolve initiator for event logging
        if ctx and ctx.identity and ctx.identity.kind == "instance":
            launcher = ctx.identity.name
        elif explicit_initiator:
            launcher = explicit_initiator
        else:
            try:
                launcher = resolve_identity().name
            except HcomError:
                launcher = "cli"

        stopped_names = []
        bg_logs = []
        for inst in tag_matches:
            name = inst["name"]
            stop_instance(name, initiated_by=launcher, reason="tag_stop")
            stopped_names.append(name)
            if inst.get("background") and inst.get("background_log_file"):
                bg_logs.append((name, inst["background_log_file"]))

        print(f"Stopped tag:{tag}: {', '.join(stopped_names)}")
        if bg_logs:
            print("\nHeadless logs:")
            for name, log_file in bg_logs:
                print(f"  {name}: {log_file}")
        return 0

    # Handle multiple explicit targets
    if len(targets) > 1:
        # Validate all targets exist first
        instances_to_stop: list[dict] = []
        not_found: list[str] = []
        for t in targets:
            if t.startswith("tag:"):
                raise CLIError(f"Cannot mix tag: with other targets: {t}")
            position = load_instance_position(t)
            if not position:
                not_found.append(t)
            else:
                instances_to_stop.append(position)

        if not_found:
            raise CLIError(f"Instance{'s' if len(not_found) > 1 else ''} not found: {', '.join(not_found)}")

        # Confirmation gate: inside AI tools, require HCOM_GO=1
        if is_inside_ai_tool() and os.environ.get("HCOM_GO") != "1":
            _print_stop_preview("multi", instances_to_stop, targets)
            return 0

        # Resolve initiator for event logging
        if ctx and ctx.identity and ctx.identity.kind == "instance":
            launcher = ctx.identity.name
        elif explicit_initiator:
            launcher = explicit_initiator
        else:
            try:
                launcher = resolve_identity().name
            except HcomError:
                launcher = "cli"

        stopped_names = []
        bg_logs = []
        for inst in instances_to_stop:
            name = inst["name"]
            # Skip remote instances in multi-stop
            if inst.get("origin_device_id"):
                print(f"Skipping remote instance: {name}")
                continue
            stop_instance(name, initiated_by=launcher, reason="multi_stop")
            stopped_names.append(name)
            if inst.get("background") and inst.get("background_log_file"):
                bg_logs.append((name, inst["background_log_file"]))

        if stopped_names:
            print(f"Stopped: {', '.join(stopped_names)}")
        if bg_logs:
            print("\nHeadless logs:")
            for name, log_file in bg_logs:
                print(f"  {name}: {log_file}")
        return 0

    # Single target or self-stop
    if targets:
        instance_name = targets[0]
    else:
        # No target - resolve identity for self-stop
        try:
            if ctx and ctx.identity:
                identity = ctx.identity
            else:
                identity = resolve_identity(name=explicit_initiator)
            instance_name = identity.name

            # Block subagents from stopping their parent
            if in_subagent_context(instance_name):
                raise CLIError("Cannot run hcom stop from within a Task subagent")
        except ValueError:
            instance_name = None

    # Handle SENDER (not real instance) - cake is real! sponge cake!
    from ..shared import SENDER

    if instance_name == SENDER:
        if IS_WINDOWS:
            raise CLIError("Cannot resolve identity - use 'hcom <n>' or Windows Terminal for stable identity")
        else:
            raise CLIError("Cannot resolve identity - launch via 'hcom <n>' for stable identity")

    # Error handling
    if not instance_name:
        raise CLIError(
            "Cannot determine identity\nUsage: hcom stop <name> | hcom stop all | run 'hcom stop' inside Claude/Gemini/Codex"
        )

    position = load_instance_position(instance_name)
    if not position:
        raise CLIError(f"'{instance_name}' not found")

    # Remote instance - send control via relay
    if position.get("origin_device_id"):
        if ":" in instance_name:
            name, device_short_id = instance_name.rsplit(":", 1)
            from ..relay import send_control

            if send_control("stop", name, device_short_id):
                print(f"Stop sent to {instance_name}")
                return 0
            else:
                raise CLIError(f"Failed to send stop to {instance_name} - relay unavailable")
        raise CLIError(f"Cannot stop remote '{instance_name}' - missing device suffix")

    # Row exists = participating (no need to check enabled)
    # Use ctx identity for initiator if available, else explicit name, else env.
    if ctx and ctx.identity and ctx.identity.kind == "instance":
        launcher = ctx.identity.name
    elif explicit_initiator:
        launcher = explicit_initiator
    else:
        try:
            launcher = resolve_identity().name
        except HcomError:
            launcher = "cli"

    # Target stop = someone stopping another instance, Self stop = no target
    is_external_stop = len(targets) > 0
    reason = "external" if is_external_stop else "self"

    # Check if this is a subagent
    if is_subagent_instance(position):
        stop_instance(instance_name, initiated_by=launcher, reason=reason)
        print(f"Stopped hcom for subagent {instance_name}.")
    else:
        # Regular parent instance
        stop_instance(instance_name, initiated_by=launcher, reason=reason)
        print(f"Stopped hcom for {instance_name}.")

    # Show background log location if applicable
    if position.get("background"):
        log_file = position.get("background_log_file", "")
        if log_file:
            print(f"\nHeadless log: {log_file}")

    return 0


def cmd_kill(argv: list[str], *, ctx: CommandContext | None = None) -> int:
    """Kill instance process (Unix only).

    Usage:
        hcom kill <name>    # Kill process group for named instance
        hcom kill all       # Kill all instances with tracked PIDs

    Only works for instances with a tracked PID (headless/background launches).
    Sends SIGTERM to the process group.
    """
    from ..core.db import iter_instances
    from ..core.instances import is_remote_instance
    import signal

    if IS_WINDOWS:
        print(format_error("hcom kill is not available on Windows"), file=sys.stderr)
        return 1

    # Identity (sender): CLI supplies ctx (preferred). Direct calls may still pass --name.
    initiator = ctx.identity.name if (ctx and ctx.identity and ctx.identity.kind == "instance") else None
    if ctx is None:
        from .utils import parse_name_flag

        from_value, argv = parse_name_flag(argv)
        initiator = from_value
    initiator = initiator if initiator else "cli"

    # Get target instance name
    args_without_flags = [a for a in argv if not a.startswith("--")]
    if not args_without_flags:
        print(format_error("Usage: hcom kill <name> | hcom kill all"), file=sys.stderr)
        return 1

    target = args_without_flags[0]

    def _kill_instance(name: str, pid: int) -> bool:
        """Kill a single instance. Returns True on success."""
        try:
            os.killpg(pid, signal.SIGTERM)
            print(f"Sent SIGTERM to process group {pid} for '{name}'")
            # Stop instance (logs event with snapshot, then deletes row)
            stop_instance(name, initiated_by=initiator, reason="killed")
            return True
        except ProcessLookupError:
            print(f"Process group {pid} not found for '{name}' (already terminated)")
            # Still stop instance to clean up DB row
            stop_instance(name, initiated_by=initiator, reason="killed")
            return True
        except PermissionError:
            print(
                format_error(f"Permission denied to kill process group {pid} for '{name}'"),
                file=sys.stderr,
            )
            return False
        except Exception as e:
            print(format_error(f"Failed to kill '{name}': {e}"), file=sys.stderr)
            return False

    # Handle 'all' target
    if target == "all":
        killed = 0
        failed = 0
        for data in iter_instances():
            # Skip remote instances (can't kill cross-device)
            # Don't skip external_sender - launching instances may have PID before session_id
            if is_remote_instance(data):
                continue
            name = data.get("name")
            pid = data.get("pid")
            if name and pid:
                if _kill_instance(name, pid):
                    killed += 1
                else:
                    failed += 1
        if killed == 0 and failed == 0:
            print("No processes with tracked PIDs found")
        else:
            print(f"Killed {killed}" + (f", {failed} failed" if failed else ""))
        return 0 if failed == 0 else 1

    # Single instance target
    position = load_instance_position(target)
    if not position:
        print(format_error(f"'{target}' not found"), file=sys.stderr)
        return 1

    pid = position.get("pid")
    if not pid:
        print(
            format_error(f"Cannot kill '{target}' - no tracked process. Use 'hcom stop {target}' instead."),
            file=sys.stderr,
        )
        return 1

    return 0 if _kill_instance(target, pid) else 1


def _start_adhoc_mode(tool: str = "adhoc", post_warning: str | None = None) -> int:
    """Start vanilla mode for external AI tools (not launched via hcom).

    Args:
        tool: Tool type ('adhoc', 'claude', 'gemini', 'codex'). Default 'adhoc' for unknown.
        post_warning: Optional warning to print after bootstrap (for tools that truncate from start)
    """
    from ..core.instances import (
        generate_unique_name,
        initialize_instance_in_position_file,
        set_status,
    )
    from ..core.db import init_db, log_event, set_session_binding
    from ..core.bootstrap import get_bootstrap

    init_db()

    # Self-healing: ensure hooks.enabled = true (required for Gemini v0.24.0+)
    # Fixes case where user ran gemini directly and settings.json is missing hooks.enabled
    if tool == "gemini":
        from ..tools.gemini.settings import ensure_hooks_enabled

        ensure_hooks_enabled()

    instance_name = generate_unique_name()

    # Get session_id from env (set by SessionStart hook via CLAUDE_ENV_FILE)
    session_id = os.environ.get("HCOM_CLAUDE_UNIX_SESSION_ID")

    # Create instance with detected tool type (row exists = participating)
    initialize_instance_in_position_file(
        instance_name,
        session_id=None,
        tool=tool,
    )

    # Capture launch context (env vars, git branch, tty)
    from ..core.instances import capture_and_store_launch_context

    capture_and_store_launch_context(instance_name)

    # Bind session if available (enables hook participation)
    if session_id:
        set_session_binding(session_id, instance_name)

    # Log started event
    log_event("life", instance_name, {"action": "started", "by": "cli", "reason": "adhoc"})

    # Set initial status context
    set_status(instance_name, "listening", "registered")

    # Print binding marker for notify hook to capture session_id
    # Format: [HCOM:BIND:<name>] - specific enough to avoid false matches
    print(f"[HCOM:BIND:{instance_name}]")

    # Print full bootstrap
    bootstrap = get_bootstrap(instance_name, tool=tool)
    print(bootstrap)

    # Mark as announced so PostToolUse hook doesn't duplicate the bootstrap
    update_instance_position(instance_name, {"name_announced": True})

    # Print warning after bootstrap (Gemini truncates from start, not end)
    if post_warning:
        print(post_warning)

    return 0


def _start_orphaned_hcom_launched() -> int:
    """Start new identity for orphaned hcom-launched instance (after stop then start).

    When an hcom-launched instance runs stop then start:
    - HCOM_PROCESS_ID and HCOM_LAUNCHED env vars still exist
    - But bindings were deleted by stop
    - Create fresh identity with new process binding
    """
    from ..core.instances import (
        generate_unique_name,
        initialize_instance_in_position_file,
        set_status,
        capture_and_store_launch_context,
    )
    from ..core.db import init_db, log_event, set_process_binding
    from ..core.bootstrap import get_bootstrap

    init_db()

    tool = detect_current_tool()
    instance_name = generate_unique_name()
    process_id = os.environ.get("HCOM_PROCESS_ID")

    # Create instance
    initialize_instance_in_position_file(
        instance_name,
        session_id=None,
        tool=tool,
    )

    capture_and_store_launch_context(instance_name)

    # Rebind process so future commands auto-resolve identity
    if process_id:
        set_process_binding(process_id, None, instance_name)

    log_event("life", instance_name, {"action": "started", "by": "cli", "reason": "restart"})

    set_status(instance_name, "listening", "registered")

    print(f"[HCOM:BIND:{instance_name}]")

    bootstrap = get_bootstrap(instance_name, tool=tool)
    print(bootstrap)

    update_instance_position(instance_name, {"name_announced": True})

    return 0


def _handle_rebind_session(rebind_target: str, current_identity: str | None) -> int:
    """Handle --as reclaim: rebind current session to a specific instance name.

    Used after session compaction/resume when AI needs to reclaim their identity.
    Always prints bootstrap since context may have been lost.
    """
    from ..core.instances import initialize_instance_in_position_file
    from ..core.db import (
        get_instance,
        delete_instance,
        get_process_binding,
        set_process_binding,
        set_session_binding,
    )

    process_id = os.environ.get("HCOM_PROCESS_ID")
    session_id = None
    if process_id:
        binding = get_process_binding(process_id)
        session_id = binding.get("session_id") if binding else None

    if not session_id and current_identity:
        current_data = get_instance(current_identity)
        if current_data:
            session_id = current_data.get("session_id")

    # Early exit: already bound to same instance
    # Still print bootstrap - context may have been lost due to compaction/resume
    if current_identity == rebind_target:
        # Ensure instance exists (may have been deleted by reset)
        if not get_instance(rebind_target):
            initialize_instance_in_position_file(rebind_target, session_id=session_id, tool=detect_current_tool())
        if session_id:
            set_session_binding(session_id, rebind_target)
        if process_id:
            set_process_binding(process_id, session_id, rebind_target)
            # Wake delivery loop to pick up restored binding
            from ..core.runtime import notify_instance

            notify_instance(rebind_target)
        print(f"[HCOM:BIND:{rebind_target}]")
        from ..core.bootstrap import get_bootstrap

        tool = detect_current_tool()
        bootstrap = get_bootstrap(rebind_target, tool=tool)
        print(bootstrap)

        return 0

    # 1. Delete target if exists (CASCADE handles session_bindings)
    # Skip remote instances (origin_device_id) - they're managed by relay
    # Preserve last_event_id so reclaimed instance resumes from where it left off
    target_data = get_instance(rebind_target)
    last_event_id = target_data.get("last_event_id") if target_data else None
    if target_data and not target_data.get("origin_device_id"):
        delete_instance(rebind_target)

    # 1b. Delete any bindings pointing to target instance
    # This ensures old PTY wrappers and hooks stop claiming this identity
    from ..core.db import (
        delete_process_bindings_for_instance,
        delete_session_bindings_for_instance,
    )

    delete_process_bindings_for_instance(rebind_target)
    delete_session_bindings_for_instance(rebind_target)  # Belt + suspenders (CASCADE should handle)

    # 2. Delete old identity (placeholder)
    if current_identity:
        delete_instance(current_identity)

    # 3. Create fresh instance
    initialize_instance_in_position_file(rebind_target, session_id=session_id, tool=detect_current_tool())

    # 4. Restore cursor position (resume from where old instance left off)
    if last_event_id:
        from ..core.instances import update_instance_position

        update_instance_position(rebind_target, {"last_event_id": last_event_id})

    # 5. Create bindings
    if session_id:
        set_session_binding(session_id, rebind_target)
    if process_id:
        set_process_binding(process_id, session_id, rebind_target)
        # Wake delivery loop to pick up restored binding
        from ..core.runtime import notify_instance

        notify_instance(rebind_target)

    print(f"[HCOM:BIND:{rebind_target}]")

    # 6. Print bootstrap (context may be lost due to compaction/resume)
    from ..core.bootstrap import get_bootstrap
    from ..core.instances import update_instance_position

    tool = detect_current_tool()
    bootstrap = get_bootstrap(rebind_target, tool=tool)
    print(bootstrap)

    # Mark as announced so hooks don't duplicate
    update_instance_position(rebind_target, {"name_announced": True})

    return 0


def cmd_start(argv: list[str], *, ctx: CommandContext | None = None) -> int:
    """Start HCOM participation.

    Usage:
        hcom start                          # Start with new identity
        hcom start --as <name>              # Reclaim identity after compaction/resume

    The --as flag is for reclaiming your existing identity when context is lost
    (e.g., after session compaction or claude --resume). It rebinds the current
    session to the specified instance name and re-prints bootstrap instructions.
    """
    from ..core.instances import initialize_instance_in_position_file, set_status
    from ..core.db import log_event

    # Validate flags before parsing
    if error := validate_flags("start", argv):
        print(format_error(error), file=sys.stderr)
        return 1

    # Identity (sender): CLI supplies ctx (preferred). Direct calls may still pass --name.
    explicit_initiator = ctx.explicit_name if ctx else None
    if ctx is None:
        from .utils import parse_name_flag

        explicit_initiator, argv = parse_name_flag(argv)

    # Extract --as flag (rebind session to existing instance)
    rebind_target = None
    i = 0
    while i < len(argv):
        if argv[i] == "--as":
            if i + 1 >= len(argv):
                raise CLIError("Usage: hcom start --as <name>")
            rebind_target = argv[i + 1]
            argv = argv[:i] + argv[i + 2 :]
            break
        i += 1

    # SUBAGENT CONTEXT BLOCK: prevent subagents from running start/start --as without proper --name
    # Must check BEFORE subagent detection to block in subagent context
    if not explicit_initiator or rebind_target:
        # No --name provided, or --as used - check if we're in subagent context via session
        try:
            from ..core.db import get_session_binding

            identity = resolve_identity()
            if identity.session_id:
                parent_instance = get_session_binding(identity.session_id)
                if parent_instance and in_subagent_context(identity.session_id):
                    if rebind_target:
                        print("[HCOM] Subagents cannot use --as. End your turn.")
                    else:
                        print(
                            "[HCOM] Cannot run 'hcom start' from within a Task subagent.\n"
                            "Subagents must use: hcom start --name <your-agent-id>"
                        )
                    return 1
        except (ValueError, HcomError):
            pass  # No identity context - allow normal flow

    # SUBAGENT DETECTION: check if --name or --as matches agent_id in parent's running_tasks
    # Must happen BEFORE --as handling to block subagents from picking new identities
    agent_id = None
    agent_type = None
    parent_name = None
    parent_session_id = None
    parent_data = None
    is_subagent = False

    # Check both --name (explicit_initiator) and --as (rebind_target) for subagent detection
    check_id = explicit_initiator or rebind_target
    if check_id:
        from ..core.db import get_db

        conn = get_db()
        rows = conn.execute(
            "SELECT name, session_id, running_tasks FROM instances WHERE running_tasks LIKE '%subagents%'"
        ).fetchall()

        for row in rows:
            running_tasks = parse_running_tasks(row["running_tasks"] or "")
            for task in running_tasks.get("subagents", []):
                if task.get("agent_id") == check_id:
                    agent_id = check_id
                    agent_type = task.get("type")
                    parent_name = row["name"]
                    parent_session_id = row["session_id"]
                    parent_data = load_instance_position(parent_name)
                    is_subagent = True
                    break
            if agent_id:
                break

    # Subagents: block ALL start variants except initial registration
    if is_subagent:
        if rebind_target:
            print("[HCOM] Subagents cannot change identity. End your turn.")
            return 1
        # Continue to subagent registration below

    # Handle --as rebind (non-subagents only)
    if rebind_target:
        from ..core.identity import is_valid_base_name, base_name_error

        if not is_valid_base_name(rebind_target):
            raise CLIError(base_name_error(rebind_target))
        current_identity = explicit_initiator
        if not current_identity:
            try:
                current_identity = resolve_identity().name
            except (ValueError, HcomError):
                current_identity = None
        return _handle_rebind_session(rebind_target, current_identity)

    # Reject positional arguments - stopped instances are deleted, nothing to restart
    args_without_flags = [a for a in argv if not a.startswith("--")]
    if args_without_flags:
        raise CLIError(f"Unknown argument: {args_without_flags[0]}\nUsage: hcom start [--as <name>]")

    if agent_id and agent_type:
        # Check if instance already exists by agent_id (reuse name)
        from ..core.db import get_db
        import sqlite3
        import re

        conn = get_db()

        # Gate: subagents get ONE start. Any stop = permanently dead.
        stopped_event = conn.execute(
            """
            SELECT json_extract(data, '$.by') as stopped_by
            FROM events
            WHERE type = 'life'
            AND json_extract(data, '$.action') = 'stopped'
            AND json_extract(data, '$.snapshot.agent_id') = ?
            ORDER BY timestamp DESC LIMIT 1
        """,
            (agent_id,),
        ).fetchone()

        if stopped_event:
            stopped_by = stopped_event["stopped_by"] or "system"
            print(
                f"[HCOM] Your session was stopped by {stopped_by}. Do not continue working. End your turn immediately."
            )
            return 1

        # Sanitize agent_type to keep subagent names valid
        agent_type = re.sub(r"[^a-z0-9_]+", "_", agent_type.lower()).strip("_")
        if not agent_type:
            agent_type = "task"
        existing = conn.execute("SELECT name FROM instances WHERE agent_id = ?", (agent_id,)).fetchone()

        if existing:
            # Already created - reuse existing name (row exists = participating)
            subagent_name = existing["name"]
            set_status(subagent_name, "active", "start")
            print(f"hcom already started for {subagent_name}")
            return 0

        # Compute next suffix: query max(n) for parent_type_% pattern
        pattern = f"{parent_name}_{agent_type}_%"
        rows = conn.execute("SELECT name FROM instances WHERE name LIKE ?", (pattern,)).fetchall()

        # Extract numeric suffixes and find max
        max_n = 0
        suffix_pattern = re.compile(rf"^{re.escape(parent_name)}_{re.escape(agent_type)}_(\d+)$")
        for row in rows:
            match = suffix_pattern.match(row["name"])
            if match:
                n = int(match.group(1))
                max_n = max(max_n, n)

        # Propose next name
        subagent_name = f"{parent_name}_{agent_type}_{max_n + 1}"

        # Single-pass insert with agent_id (direct DB insert, not via initialize_instance_in_position_file)
        import time
        from ..core.db import get_last_event_id

        initial_event_id = get_last_event_id() if SKIP_HISTORY else 0
        parent_tag = parent_data.get("tag") or None

        try:
            conn.execute(
                """INSERT INTO instances (name, session_id, parent_session_id, parent_name, tag, agent_id, created_at, last_event_id, directory, last_stop, status)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                (
                    subagent_name,
                    None,
                    parent_session_id,
                    parent_name,
                    parent_tag,
                    agent_id,
                    time.time(),
                    initial_event_id,
                    str(Path.cwd()),
                    0,
                    "active",
                ),
            )
            conn.commit()
        except sqlite3.IntegrityError as e:
            # Unexpected collision - retry once with next suffix
            subagent_name = f"{parent_name}_{agent_type}_{max_n + 2}"
            try:
                conn.execute(
                    """INSERT INTO instances (name, session_id, parent_session_id, parent_name, tag, agent_id, created_at, last_event_id, directory, last_stop, status)
                       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                    (
                        subagent_name,
                        None,
                        parent_session_id,
                        parent_name,
                        parent_tag,
                        agent_id,
                        time.time(),
                        initial_event_id,
                        str(Path.cwd()),
                        0,
                        "active",
                    ),
                )
                conn.commit()
            except sqlite3.IntegrityError:
                print(
                    f"Error: Failed to create unique name after retry: {e}",
                    file=sys.stderr,
                )
                return 1

        # Capture launch context (env vars, git branch, tty)
        from ..core.instances import capture_and_store_launch_context

        capture_and_store_launch_context(subagent_name)

        # Set active status
        set_status(subagent_name, "active", "tool:start")

        # Push subagent creation to relay
        try:
            from ..relay import notify_relay_tui, push

            if not notify_relay_tui():
                push()
        except Exception:
            pass

        # Print subagent bootstrap
        from ..core.bootstrap import get_subagent_bootstrap

        result = get_subagent_bootstrap(subagent_name, parent_name, agent_id)
        if result:
            print(result)
        return 0

    # Skip redirect if --name is provided with an existing instance (allows resume use case)
    has_valid_identity = explicit_initiator and load_instance_position(explicit_initiator)

    # Binding hierarchy (capabilities increase up):
    #   adhoc         - no bindings, manual polling only
    #   session bound - PostToolUse hooks for mid-turn delivery, transcript/status tracking
    #   process bound - PTY wrapper adds idle injection (push when AI is waiting)
    # Vanilla AI tools get session binding via PostToolUse marker, but no idle injection.
    if not has_valid_identity:
        vanilla_tool = detect_vanilla_tool()

        # Auto-install hooks if missing for the detected tool
        if vanilla_tool:
            hooks_installed = _verify_hooks_for_tool(vanilla_tool)
            if not hooks_installed:
                tool_display = {
                    "claude": "Claude Code",
                    "gemini": "Gemini CLI",
                    "codex": "Codex",
                }.get(vanilla_tool, vanilla_tool)
                # Auto-install hooks
                from .hooks_cmd import cmd_hooks_add

                print(f"Installing {vanilla_tool} hooks...")
                if cmd_hooks_add([vanilla_tool]) == 0:
                    print(f"\nRestart {tool_display} to enable automatic message delivery.")
                    print("Then run: hcom start")
                else:
                    print(
                        f"Failed to install hooks. Run: hcom hooks add {vanilla_tool}",
                        file=sys.stderr,
                    )
                return 1

        if vanilla_tool == "claude":
            # Claude hooks handle everything - no warning needed
            return _start_adhoc_mode(tool="claude")
        elif vanilla_tool in ("codex", "gemini"):
            # Session-bound but missing idle injection - warn human (before and after bootstrap)
            warning = (
                f"{BOLD}{FG_YELLOW}No idle push message delivery. For full experience, run: hcom {vanilla_tool}{RESET}"
            )
            print(warning)
            return _start_adhoc_mode(tool=vanilla_tool, post_warning=warning)

    # Resolve identity
    try:
        # Use --name for self-start if provided (confirms existing identity)
        instance_name = resolve_identity(name=explicit_initiator).name
    except (ValueError, HcomError) as e:
        # Re-raise if it's a specific actionable error (like "not found")
        if "not found" in str(e).lower():
            raise
        instance_name = None

    # Handle SENDER (CLI call outside Claude Code)
    # CLAUDECODE != '1' means not inside Claude Code → AI tool wanting adhoc mode
    from ..shared import SENDER

    if instance_name == SENDER:
        return _start_adhoc_mode()

    # Error handling - no instance_name resolved
    if not instance_name:
        # Check if this is an external AI tool wanting adhoc mode
        if not is_inside_ai_tool():
            return _start_adhoc_mode()

        # Orphaned hcom-launched: env vars exist but bindings deleted (stop then start)
        process_id = os.environ.get("HCOM_PROCESS_ID")
        hcom_launched = os.environ.get("HCOM_LAUNCHED") == "1"
        if process_id or hcom_launched:
            return _start_orphaned_hcom_launched()

        print(format_error("Cannot determine identity"), file=sys.stderr)
        print(
            "Usage: hcom start | run inside Claude/Gemini/Codex | use 'hcom <count>' to launch",
            file=sys.stderr,
        )
        return 1

    # Load or create instance
    existing_data = load_instance_position(instance_name) if instance_name else None

    # Remote instance - send control via relay
    if existing_data and existing_data.get("origin_device_id"):
        if ":" in instance_name:
            name, device_short_id = instance_name.rsplit(":", 1)
            from ..relay import send_control

            if send_control("start", name, device_short_id):
                print(f"Start sent to {instance_name}")
                return 0
            else:
                raise CLIError(f"Failed to send start to {instance_name} - relay unavailable")
        raise CLIError(f"Cannot start remote '{instance_name}' - missing device suffix")

    # Handle non-existent instance - create new one
    if not existing_data:
        if not instance_name:
            from ..core.instances import generate_unique_name

            instance_name = generate_unique_name()

        initialize_instance_in_position_file(instance_name, None, tool=detect_current_tool())

        if explicit_initiator:
            launcher = explicit_initiator
        else:
            try:
                launcher = resolve_identity().name
            except HcomError:
                launcher = "cli"
        log_event(
            "life",
            instance_name,
            {"action": "started", "by": launcher, "reason": "cli"},
        )
        print(f"[HCOM:BIND:{instance_name}]")
        print(f"Started hcom for {instance_name}")

        return 0

    # Row exists - but check if it's actually usable for AI tools
    from ..core.db import has_session_binding

    if not has_session_binding(instance_name) and is_inside_ai_tool():
        # AI tool context: row exists but no session binding (inactive from previous session)
        # Suggest --as to rebind this session to the instance
        status = existing_data.get("status", "inactive")
        from ..core.tool_utils import build_hcom_command

        hcom_cmd = build_hcom_command()
        print(f"'{instance_name}' exists but is {status} (no active session).")
        print(f"To rebind this session to '{instance_name}', run:")
        print(f"  {hcom_cmd} start --as {instance_name}")
        return 1

    # Active session binding exists, or ad-hoc CLI usage
    print(f"hcom already started for {instance_name}")
    return 0


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

    from ..launcher import launch as unified_launch
    from ..core.config import get_config

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
    from ..tools.gemini.args import resolve_gemini_args, merge_gemini_args

    env_spec = resolve_gemini_args(None, get_config().gemini_args)
    cli_spec = resolve_gemini_args(argv, None)
    spec = merge_gemini_args(env_spec, cli_spec) if (cli_spec.clean_tokens or cli_spec.positional_tokens) else env_spec

    # Validate parsed args (strict by default, overridable).
    from ..shared import skip_tool_args_validation, HCOM_SKIP_TOOL_ARGS_VALIDATION_ENV

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
    if is_inside_ai_tool() and os.environ.get("HCOM_GO") != "1":
        _print_launch_preview("gemini", count, False, argv)  # Gemini always interactive
        return 0

    # Check for --no-auto-watch flag (used by TUI to prevent opening another watch window)
    no_auto_watch = "--no-auto-watch" in argv
    if no_auto_watch:
        argv = [arg for arg in argv if arg != "--no-auto-watch"]

    # Build final args from merged spec
    gemini_args = spec.rebuild_tokens()

    # Determine if instance will run in current terminal (blocking mode)
    from ..launcher import will_run_in_current_terminal

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

    for h in result.get("handles", []):
        instance_name = h.get("instance_name")
        if instance_name:
            print(f"Started the launch process for Gemini: {instance_name}")

    print(f"\nStarted the launch process for {launched} Gemini agent{'s' if launched != 1 else ''}")
    print(f"Batch id: {result['batch_id']}")
    print("To block until ready or fail, run: hcom events launch")

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

        from ..ui import run_tui

        return run_tui(hcom_path())
    else:
        tips = []
        tips.append("Instance names shown in hcom list after startup")
        tips.append("Send message: hcom send '@<name> hello'")
        print("\n" + "\n".join(f"  • {tip}" for tip in tips) + "\n")

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

    from ..launcher import launch as unified_launch
    from ..tools.codex.args import resolve_codex_args
    from ..core.config import get_config

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
    from ..tools.codex.args import merge_codex_args

    env_spec = resolve_codex_args(None, get_config().codex_args)
    cli_spec = resolve_codex_args(argv, None)
    spec = (
        merge_codex_args(env_spec, cli_spec)
        if (cli_spec.clean_tokens or cli_spec.positional_tokens or cli_spec.subcommand)
        else env_spec
    )

    # Validate parsed args (strict by default, overridable).
    from ..shared import skip_tool_args_validation, HCOM_SKIP_TOOL_ARGS_VALIDATION_ENV

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
    if is_inside_ai_tool() and os.environ.get("HCOM_GO") != "1":
        _print_launch_preview("codex", count, False, argv)  # Codex always interactive
        return 0

    # Handle resume/fork subcommand
    if spec.subcommand in ("resume", "fork"):
        if not spec.positional_tokens:
            raise CLIError(
                f"'codex {spec.subcommand}' requires explicit thread-id (interactive picker not supported)"
            )
        if spec.has_flag(["--last"]):
            raise CLIError(
                f"'codex {spec.subcommand} --last' not supported - use explicit thread-id"
            )
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
    from ..launcher import will_run_in_current_terminal

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
    print("To block until ready or fail, run: hcom events launch")

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

        from ..ui import run_tui

        return run_tui(hcom_path())
    else:
        tips = []
        if instance_names:
            tips.append(f"Send message: hcom send '@{instance_names[0]} hello'")
        if launched > 0:
            tips.append("Check status: hcom list")
        if tips:
            print("\n" + "\n".join(f"  • {tip}" for tip in tips) + "\n")

    return 0 if failed == 0 else 1


def _do_resume(name: str, prompt: str | None = None, *, run_here: bool | None = None) -> int:
    """Resume an instance by launching tool with --resume and session_id.

    Used by TUI [R] key. Launches in the instance's original directory.

    Args:
        name: Instance name to resume
        prompt: Optional prompt to pass to resumed instance
        run_here: If False, force new terminal window. If None, use default logic.
    """
    from ..launcher import launch as unified_launch

    # Load instance data
    instance_data = load_instance_position(name)
    if not instance_data:
        raise CLIError(f"'{name}' not found")

    session_id = instance_data.get("session_id")
    if not session_id:
        raise CLIError(f"'{name}' has no session_id (cannot resume)")

    tool = instance_data.get("tool", "claude")
    is_headless = instance_data.get("background", False)
    original_dir = instance_data.get("directory") or os.getcwd()

    # Default prompt instructs instance to reclaim identity
    if prompt is None:
        prompt = f"run hcom start --as {name}"

    # Build args based on tool type (session_id only, no stored launch_args)
    if tool == "claude":
        args = ["--resume", session_id]
        if is_headless:
            args.append("-p")
        if prompt:
            args.append(prompt)

    elif tool == "gemini":
        args = ["--resume", session_id]

    elif tool == "codex":
        args = ["resume", session_id]

    else:
        raise CLIError(f"Resume not supported for tool: {tool}")

    # Get launcher name
    try:
        launcher_name = resolve_identity().name
    except Exception:
        launcher_name = "user"

    # PTY mode: use PTY wrapper for interactive Claude (not headless, not Windows)
    use_pty = tool == "claude" and not is_headless and not IS_WINDOWS

    # Launch in original directory
    result = unified_launch(
        tool,
        1,
        args,
        launcher=launcher_name,
        background=is_headless,
        prompt=prompt if is_headless else None,
        run_here=run_here,
        cwd=original_dir,
        pty=use_pty,
    )

    launched = result["launched"]
    if launched == 1:
        print(f"Resumed {name} ({tool})")
        return 0
    else:
        return 1
