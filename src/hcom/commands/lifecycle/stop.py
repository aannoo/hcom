"""Stop and kill commands."""

import sys

from ..utils import (
    CLIError,
    format_error,
    resolve_identity,
    validate_flags,
)
from ...shared import (
    IS_WINDOWS,
    HcomError,
    is_inside_ai_tool,
    CommandContext,
)
from ...core.thread_context import get_hcom_go
from ...core.instances import (
    get_full_name,
    load_instance_position,
    resolve_display_name,
    is_subagent_instance,
)
from ...tools.claude.subagent import in_subagent_context
from ...core.db import iter_instances
from ...core.tool_utils import stop_instance, build_hcom_command


def _print_stop_preview(target: str, instances: list[dict], target_names: list[str] | None = None) -> None:
    """Print stop preview for AI tools. Shows what will be stopped.

    Args:
        target: 'all', 'tag:name', or 'multi' for multiple explicit targets
        instances: List of instance dicts to stop
        target_names: For 'multi' mode, the original target names from CLI
    """
    hcom_cmd = build_hcom_command()
    count = len(instances)
    names = [get_full_name(i) or i["name"] for i in instances]

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
    from ...core.log import log_info
    # Validate flags
    if error := validate_flags("stop", argv):
        print(format_error(error), file=sys.stderr)
        return 1

    # Identity (sender): CLI supplies ctx (preferred). Direct calls may still pass --name.
    explicit_initiator = ctx.explicit_name if ctx else None
    if ctx is None:
        from ..utils import parse_name_flag

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
        if is_inside_ai_tool() and not get_hcom_go():
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
        log_info("lifecycle", "stop.all", count=len(instances), initiated_by=launcher)
        for instance_data in instances:
            # Row exists = participating (stop all instances)
            instance_name = instance_data["name"]
            display_name = get_full_name(instance_data) or instance_name
            stop_instance(instance_name, initiated_by=launcher, reason="stop_all")
            stopped_names.append(display_name)
            stopped_count += 1

            # Track background logs
            if instance_data.get("background"):
                log_file = instance_data.get("background_log_file", "")
                if log_file:
                    bg_logs.append((display_name, log_file))

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
        if is_inside_ai_tool() and not get_hcom_go():
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
        log_info("lifecycle", "stop.tag", tag=tag, count=len(tag_matches), initiated_by=launcher)
        for inst in tag_matches:
            name = inst["name"]
            display_name = get_full_name(inst) or name
            stop_instance(name, initiated_by=launcher, reason="tag_stop")
            stopped_names.append(display_name)
            if inst.get("background") and inst.get("background_log_file"):
                bg_logs.append((display_name, inst["background_log_file"]))

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
            resolved = resolve_display_name(t)
            position = load_instance_position(resolved) if resolved else {}
            if not position:
                not_found.append(t)
            else:
                instances_to_stop.append(position)  # type: ignore[arg-type]

        if not_found:
            raise CLIError(f"Instance{'s' if len(not_found) > 1 else ''} not found: {', '.join(not_found)}")

        # Confirmation gate: inside AI tools, require HCOM_GO=1
        if is_inside_ai_tool() and not get_hcom_go():
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
            display_name = get_full_name(inst) or name
            # Skip remote instances in multi-stop
            if inst.get("origin_device_id"):
                print(f"Skipping remote instance: {display_name}")
                continue
            stop_instance(name, initiated_by=launcher, reason="multi_stop")
            stopped_names.append(display_name)
            if inst.get("background") and inst.get("background_log_file"):
                bg_logs.append((display_name, inst["background_log_file"]))

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
    from ...shared import SENDER

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

    resolved = resolve_display_name(instance_name)
    if resolved:
        instance_name = resolved
    position = load_instance_position(instance_name)
    if not position:
        raise CLIError(f"'{instance_name}' not found")

    # Remote instance - send control via relay
    if position.get("origin_device_id"):
        if ":" in instance_name:
            name, device_short_id = instance_name.rsplit(":", 1)
            from ...relay import send_control

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
    display_name = get_full_name(position) or instance_name
    log_info("lifecycle", "stop.single", name=instance_name, reason=reason, initiated_by=launcher)
    if is_subagent_instance(position):
        stop_instance(instance_name, initiated_by=launcher, reason=reason)
        print(f"Stopped hcom for subagent {display_name}.")
    else:
        # Regular parent instance
        stop_instance(instance_name, initiated_by=launcher, reason=reason)
        print(f"Stopped hcom for {display_name}.")

    # Show background log location if applicable
    if position.get("background"):
        log_file = position.get("background_log_file", "")
        if log_file:
            print(f"\nHeadless log: {log_file}")

    return 0


def cmd_kill(argv: list[str], *, ctx: CommandContext | None = None) -> int:
    """Kill instance process (Unix only).

    Usage:
        hcom kill <name>       # Kill process group for named instance
        hcom kill tag:<name>   # Kill all instances with tag
        hcom kill all          # Kill all instances with tracked PIDs

    Only works for instances with a tracked PID (headless/background launches).
    Sends SIGTERM to the process group.
    """
    from ...core.db import iter_instances
    from ...core.instances import is_remote_instance

    if IS_WINDOWS:
        print(format_error("hcom kill is not available on Windows"), file=sys.stderr)
        return 1

    # Identity (sender): CLI supplies ctx (preferred). Direct calls may still pass --name.
    initiator = ctx.identity.name if (ctx and ctx.identity and ctx.identity.kind == "instance") else None
    if ctx is None:
        from ..utils import parse_name_flag

        from_value, argv = parse_name_flag(argv)
        initiator = from_value
    initiator = initiator if initiator else "cli"

    # Get target instance name
    args_without_flags = [a for a in argv if not a.startswith("--")]
    if not args_without_flags:
        print(format_error("Usage: hcom kill <name> | hcom kill tag:<name> | hcom kill all"), file=sys.stderr)
        return 1

    target = args_without_flags[0]

    def _kill_instance(name: str, pid: int, display: str = "", background: bool = False) -> bool:
        """Kill a single instance. Returns True on success."""
        from ...core.log import log_info, log_warn
        from ...terminal import KillResult, kill_process, resolve_terminal_info

        show = display or name
        # Headless instances have no terminal pane — skip pane close entirely
        if background:
            result, pane_closed = kill_process(pid)
        else:
            info = resolve_terminal_info(name, pid)
            result, pane_closed = kill_process(pid, preset_name=info.preset_name, pane_id=info.pane_id,
                                  process_id=info.process_id, kitty_listen_on=info.kitty_listen_on,
                                  terminal_id=info.terminal_id)
        if background:
            preset_name = ""
            pane_id = ""
        else:
            preset_name = info.preset_name
            pane_id = info.pane_id
        if result == KillResult.PERMISSION_DENIED:
            log_warn("lifecycle", "kill.permission_denied", name=name, pid=pid)
            print(format_error(f"Permission denied to kill process group {pid} for '{show}'"), file=sys.stderr)
            return False
        log_info("lifecycle", "kill", name=name, pid=pid, result=result.name, pane_closed=pane_closed)
        if pane_closed:
            pane_info = f" (closed {preset_name} pane {pane_id})" if pane_id else f" (closed {preset_name} pane)"
        elif preset_name:
            pane_info = f" (pane close failed for {preset_name})"
        else:
            pane_info = ""
        if result == KillResult.ALREADY_DEAD:
            print(f"Process group {pid} not found for '{show}' (already terminated){pane_info}")
        else:
            print(f"Sent SIGTERM to process group {pid} for '{show}'{pane_info}")
        stop_instance(name, initiated_by=initiator, reason="killed")
        return True

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
                if _kill_instance(name, pid, get_full_name(data) or name, background=bool(data.get("background"))):
                    killed += 1
                else:
                    failed += 1
        # Also kill orphan processes (stopped but still running)
        from ...core.pidtrack import get_orphan_processes, remove_pid
        from ...terminal import KillResult, kill_process

        for orphan in get_orphan_processes():
            pid = orphan["pid"]
            o_preset = orphan.get("terminal_preset", "")
            o_pane = orphan.get("pane_id", "")
            result, pane_closed = kill_process(pid, preset_name=o_preset, pane_id=o_pane,
                                               process_id=orphan.get("process_id", ""),
                                               kitty_listen_on=orphan.get("kitty_listen_on", ""),
                                               terminal_id=orphan.get("terminal_id", ""))
            if result == KillResult.PERMISSION_DENIED:
                failed += 1
                continue
            names = ", ".join(orphan.get("names", []))
            if pane_closed:
                pane_info = f", closed {o_preset} pane {o_pane}" if o_pane else f", closed {o_preset} pane"
            elif o_preset:
                pane_info = f", pane close failed for {o_preset}"
            else:
                pane_info = ""
            label = f" ({names}{pane_info})" if names or pane_info else ""
            if result == KillResult.ALREADY_DEAD:
                print(f"Orphan process group {pid} already terminated{label}")
            else:
                print(f"Sent SIGTERM to orphan process group {pid}{label}")
            killed += 1
            remove_pid(pid)

        if killed == 0 and failed == 0:
            print("No processes with tracked PIDs found")
        else:
            print(f"Killed {killed}" + (f", {failed} failed" if failed else ""))
        return 0 if failed == 0 else 1

    # Handle tag:name syntax - kill all instances with matching tag
    if target.startswith("tag:"):
        tag = target[4:]
        tag_matches = [i for i in iter_instances() if i.get("tag") == tag and not is_remote_instance(i)]
        if not tag_matches:
            print(format_error(f"No instances with tag '{tag}'"), file=sys.stderr)
            return 1
        killed = 0
        failed = 0
        for inst in tag_matches:
            name = inst.get("name")
            show = get_full_name(inst) or name
            pid = inst.get("pid")
            if name and pid:
                if _kill_instance(name, pid, show or name, background=bool(inst.get("background"))):
                    killed += 1
                else:
                    failed += 1
            elif name:
                print(format_error(f"Cannot kill '{show or name}' - no tracked process. Use 'hcom stop {name}' instead."),
                      file=sys.stderr)
                failed += 1
        if killed == 0 and failed == 0:
            print(f"No processes with tracked PIDs for tag '{tag}'")
        else:
            print(f"Killed {killed} with tag '{tag}'" + (f", {failed} failed" if failed else ""))
        return 0 if failed == 0 else 1

    # Single instance target (accept base name or tag-name)
    resolved = resolve_display_name(target)
    if resolved:
        target = resolved
    position = load_instance_position(target)
    if position:
        pid = position.get("pid")
        show = get_full_name(position) or target
        if not pid:
            print(
                format_error(f"Cannot kill '{show}' - no tracked process. Use 'hcom stop {target}' instead."),
                file=sys.stderr,
            )
            return 1
        return 0 if _kill_instance(target, pid, show, background=bool(position.get("background"))) else 1

    # Not an active instance — check orphan processes (stopped but still running)
    from ...core.pidtrack import get_orphan_processes, remove_pid

    orphans = get_orphan_processes()
    for orphan in orphans:
        if target in orphan.get("names", []):
            pid = orphan["pid"]
            from ...terminal import KillResult, kill_process
            o_preset = orphan.get("terminal_preset", "")
            o_pane = orphan.get("pane_id", "")
            result, pane_closed = kill_process(pid, preset_name=o_preset, pane_id=o_pane,
                                               process_id=orphan.get("process_id", ""),
                                               kitty_listen_on=orphan.get("kitty_listen_on", ""),
                                               terminal_id=orphan.get("terminal_id", ""))
            if result == KillResult.PERMISSION_DENIED:
                print(format_error(f"Permission denied to kill process group {pid}"), file=sys.stderr)
                return 1
            if pane_closed:
                pane_info = f" (closed {o_preset} pane {o_pane})" if o_pane else f" (closed {o_preset} pane)"
            elif o_preset:
                pane_info = f" (pane close failed for {o_preset})"
            else:
                pane_info = ""
            if result == KillResult.ALREADY_DEAD:
                print(f"Process group {pid} not found for '{target}' (already terminated){pane_info}")
            else:
                print(f"Sent SIGTERM to process group {pid} for stopped instance '{target}'{pane_info}")
            remove_pid(pid)
            return 0

    print(format_error(f"'{target}' not found"), file=sys.stderr)
    return 1
