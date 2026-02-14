"""Start command and helpers."""

import os
import sys

from ..utils import (
    CLIError,
    format_error,
    resolve_identity,
    validate_flags,
)
from ...shared import (
    BOLD,
    FG_YELLOW,
    RESET,
    HcomError,
    is_inside_ai_tool,
    detect_vanilla_tool,
    detect_current_tool,
    CommandContext,
    ST_ACTIVE,
    ST_LISTENING,
    ST_INACTIVE,
)
from ...core.thread_context import get_process_id, get_is_launched, get_cwd
from ...core.instances import (
    load_instance_position,
    update_instance_position,
    parse_running_tasks,
)
from .launch import _verify_hooks_for_tool


def _start_adhoc_mode(tool: str = "adhoc", post_warning: str | None = None) -> int:
    """Start vanilla mode for external AI tools (not launched via hcom).

    Args:
        tool: Tool type ('adhoc', 'claude', 'gemini', 'codex'). Default 'adhoc' for unknown.
        post_warning: Optional warning to print after bootstrap (for tools that truncate from start)
    """
    from ...core.instances import (
        generate_unique_name,
        initialize_instance_in_position_file,
        set_status,
    )
    from ...core.db import init_db, log_event, set_session_binding
    from ...core.bootstrap import get_bootstrap

    from ...core.log import log_info

    init_db()

    # Self-healing: ensure hooks.enabled = true (required for Gemini v0.24.0+)
    # Fixes case where user ran gemini directly and settings.json is missing hooks.enabled
    if tool == "gemini":
        from ...tools.gemini.settings import ensure_hooks_enabled

        ensure_hooks_enabled()

    instance_name = generate_unique_name()
    log_info("lifecycle", "start.adhoc", name=instance_name, tool=tool)

    # Get session_id from env
    # Claude: HCOM_CLAUDE_UNIX_SESSION_ID (SessionStart hook writes to CLAUDE_ENV_FILE)
    # Codex: CODEX_THREAD_ID (injected by Codex into all child process envs)
    from ...core.thread_context import get_codex_thread_id

    session_id = os.environ.get("HCOM_CLAUDE_UNIX_SESSION_ID") or get_codex_thread_id()

    # Create instance with detected tool type (row exists = participating)
    initialize_instance_in_position_file(
        instance_name,
        session_id=None,
        tool=tool,
    )

    # Capture launch context (env vars, git branch, tty)
    from ...core.instances import capture_and_store_launch_context

    capture_and_store_launch_context(instance_name)

    # Bind session if available (enables hook participation)
    if session_id:
        set_session_binding(session_id, instance_name)

    # Log started event
    log_event("life", instance_name, {"action": "started", "by": "cli", "reason": "adhoc"})

    # Set initial status context
    set_status(instance_name, ST_LISTENING, "registered")

    # Print binding marker for notify hook to capture session_id
    # Format: [hcom:<name>] - specific enough to avoid false matches
    print(f"[hcom:{instance_name}]")

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
    from ...core.instances import (
        generate_unique_name,
        initialize_instance_in_position_file,
        set_status,
        capture_and_store_launch_context,
    )
    from ...core.db import init_db, log_event, set_process_binding
    from ...core.bootstrap import get_bootstrap

    from ...core.log import log_info

    init_db()

    tool = detect_current_tool()
    instance_name = generate_unique_name()
    process_id = get_process_id()
    log_info("lifecycle", "start.orphaned_restart", name=instance_name, tool=tool, process_id=process_id)

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

    # Recover PID from orphan tracking (PTY process survives stop)
    from ...core.pidtrack import recover_orphan_pid

    recover_orphan_pid(instance_name, process_id)

    log_event("life", instance_name, {"action": "started", "by": "cli", "reason": "restart"})

    set_status(instance_name, ST_LISTENING, "registered")

    print(f"[hcom:{instance_name}]")

    bootstrap = get_bootstrap(instance_name, tool=tool)
    print(bootstrap)

    update_instance_position(instance_name, {"name_announced": True})

    return 0


def _handle_rebind_session(rebind_target: str, current_identity: str | None) -> int:
    """Handle --as reclaim: rebind current session to a specific instance name.

    Used after session compaction/resume when AI needs to reclaim their identity.
    Always prints bootstrap since context may have been lost.
    """
    from ...core.log import log_info
    from ...core.instances import initialize_instance_in_position_file
    from ...core.db import (
        get_instance,
        delete_instance,
        get_process_binding,
        set_process_binding,
        set_session_binding,
        migrate_notify_endpoints,
    )

    process_id = get_process_id()
    session_id = None
    if process_id:
        binding = get_process_binding(process_id)
        session_id = binding.get("session_id") if binding else None

    if not session_id and current_identity:
        current_data = get_instance(current_identity)
        if current_data:
            session_id = current_data.get("session_id")

    log_info("lifecycle", "start.rebind", target=rebind_target, current=current_identity, process_id=process_id, has_session=bool(session_id))

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
            from ...core.runtime import notify_instance

            notify_instance(rebind_target)
        print(f"[hcom:{rebind_target}]")
        from ...core.bootstrap import get_bootstrap

        tool = detect_current_tool()
        bootstrap = get_bootstrap(rebind_target, tool=tool)
        print(bootstrap)

        return 0

    # 1. Delete target if exists (CASCADE handles session_bindings)
    # Skip remote instances (origin_device_id) - they're managed by relay
    # Preserve last_event_id so reclaimed instance resumes from where it left off
    target_data = get_instance(rebind_target)
    last_event_id = target_data.get("last_event_id") if target_data else None
    # Fallback: read cursor from stopped snapshot if instance row was already deleted
    if last_event_id is None:
        from .resume import _load_stopped_snapshot

        stopped = _load_stopped_snapshot(rebind_target)
        if stopped:
            last_event_id = stopped.get("last_event_id")
    # Final fallback: if both row and snapshot are gone (e.g., stop_instance failed
    # to log snapshot due to DB lock), use current max to avoid re-delivering old messages.
    # Without this, initialize_instance_in_position_file uses stale HCOM_LAUNCH_EVENT_ID.
    if last_event_id is None:
        from ...core.db import get_last_event_id as _get_last_event_id

        last_event_id = _get_last_event_id()
    if target_data and not target_data.get("origin_device_id"):
        delete_instance(rebind_target)

    # 1b. Delete any bindings pointing to target instance
    # This ensures old PTY wrappers and hooks stop claiming this identity
    from ...core.db import (
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

    # 3b. Recover PID from orphan tracking (PTY process survives stop)
    from ...core.pidtrack import recover_orphan_pid

    recover_orphan_pid(rebind_target, process_id)

    # 4. Restore cursor position (resume from where old instance left off)
    if last_event_id is not None:
        update_instance_position(rebind_target, {"last_event_id": last_event_id})

    # 5. Create bindings
    if session_id:
        set_session_binding(session_id, rebind_target)
    if process_id:
        set_process_binding(process_id, session_id, rebind_target)
        # Migrate notify/inject endpoints before notify so the wake reaches the right port
        if current_identity and current_identity != rebind_target:
            migrate_notify_endpoints(current_identity, rebind_target)
        from ...core.runtime import notify_instance

        notify_instance(rebind_target)

    print(f"[hcom:{rebind_target}]")

    # 6. Print bootstrap (context may be lost due to compaction/resume)
    from ...core.bootstrap import get_bootstrap

    tool = detect_current_tool()
    bootstrap = get_bootstrap(rebind_target, tool=tool)
    print(bootstrap)

    # Mark as announced so hooks don't duplicate
    update_instance_position(rebind_target, {"name_announced": True})

    # Push rebound instance to relay (hooks will set_status shortly, but clear stale state now)
    try:
        from ...relay import trigger_push

        trigger_push()
    except Exception:
        pass

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
    from ...core.instances import initialize_instance_in_position_file, set_status
    from ...core.db import log_event

    # Validate flags before parsing
    if error := validate_flags("start", argv):
        print(format_error(error), file=sys.stderr)
        return 1

    # Identity (sender): CLI supplies ctx (preferred). Direct calls may still pass --name.
    explicit_initiator = ctx.explicit_name if ctx else None
    if ctx is None:
        from ..utils import parse_name_flag

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

    # BLOCK DURING ACTIVE TASKS: prevents subagents from corrupting parent/sibling instances
    # When subagent runs --as or bare start, process_id resolves to parent which has running_tasks.active=True
    if rebind_target or not explicit_initiator:
        try:
            identity = resolve_identity()
            if identity.instance_data:
                running_tasks = parse_running_tasks(identity.instance_data.get("running_tasks", ""))
                if running_tasks.get("active"):
                    if rebind_target:
                        print("[HCOM] Cannot use --as while Tasks are running.")
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
        from ...core.db import get_db

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
        from ...core.identity import is_valid_base_name, base_name_error

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
        from ...core.db import get_db
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
            set_status(subagent_name, ST_ACTIVE, "start")
            print(f"hcom already started for {subagent_name}")
            return 0

        # Compute next suffix: query max(n) for parent_type_% pattern
        pattern = f"{parent_name}_{agent_type}_%"
        rows = conn.execute("SELECT name FROM instances WHERE name LIKE ?", (pattern,)).fetchall()

        # Extract numeric suffixes and find max
        max_n = 0
        suffix_pattern = re.compile(rf"^{re.escape(parent_name or '')}_{re.escape(agent_type or '')}_(\d+)$")  # type: ignore[type-var]
        for row in rows:
            match = suffix_pattern.match(row["name"])
            if match:
                n = int(match.group(1))
                max_n = max(max_n, n)

        # Propose next name
        subagent_name = f"{parent_name}_{agent_type}_{max_n + 1}"

        # Single-pass insert with agent_id (direct DB insert, not via initialize_instance_in_position_file)
        import time
        from ...core.db import get_last_event_id

        initial_event_id = get_last_event_id()
        parent_tag = parent_data.get("tag") if parent_data else None

        try:
            conn.execute(
                """INSERT INTO instances
                   (name, session_id, parent_session_id, parent_name, tag, agent_id,
                    created_at, last_event_id, directory, last_stop, status)
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
                    str(get_cwd()),
                    0,
                    ST_ACTIVE,
                ),
            )
            conn.commit()
        except sqlite3.IntegrityError as e:
            # Unexpected collision - retry once with next suffix
            subagent_name = f"{parent_name}_{agent_type}_{max_n + 2}"
            try:
                conn.execute(
                    """INSERT INTO instances
                       (name, session_id, parent_session_id, parent_name, tag, agent_id,
                        created_at, last_event_id, directory, last_stop, status)
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
                        str(get_cwd()),
                        0,
                        ST_ACTIVE,
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
        from ...core.instances import capture_and_store_launch_context

        capture_and_store_launch_context(subagent_name)

        # Set active status
        set_status(subagent_name, ST_ACTIVE, "tool:start")

        from ...core.log import log_info as _log_info
        _log_info("lifecycle", "start.subagent", name=subagent_name, parent=parent_name, agent_id=agent_id, agent_type=agent_type)

        # Print subagent bootstrap
        from ...core.bootstrap import get_subagent_bootstrap

        result = get_subagent_bootstrap(subagent_name, parent_name or "")
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
                from ..hooks_cmd import cmd_hooks_add

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
    from ...shared import SENDER

    if instance_name == SENDER:
        return _start_adhoc_mode()

    # Error handling - no instance_name resolved
    if not instance_name:
        # Check if this is an external AI tool wanting adhoc mode
        if not is_inside_ai_tool():
            return _start_adhoc_mode()

        # Orphaned hcom-launched: env vars exist but bindings deleted (stop then start)
        process_id = get_process_id()
        hcom_launched = get_is_launched()
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
            from ...relay import send_control

            if send_control("start", name, device_short_id):
                print(f"Start sent to {instance_name}")
                return 0
            else:
                raise CLIError(f"Failed to send start to {instance_name} - relay unavailable")
        raise CLIError(f"Cannot start remote '{instance_name}' - missing device suffix")

    # Handle non-existent instance - create new one
    if not existing_data:
        if not instance_name:
            from ...core.instances import generate_unique_name

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
        print(f"[hcom:{instance_name}]")
        print(f"Started hcom for {instance_name}")

        return 0

    # Row exists - but check if it's actually usable for AI tools
    from ...core.db import has_session_binding

    if not has_session_binding(instance_name) and is_inside_ai_tool():
        # AI tool context: row exists but no session binding (inactive from previous session)
        # Suggest --as to rebind this session to the instance
        status = existing_data.get("status", ST_INACTIVE)
        from ...core.tool_utils import build_hcom_command

        hcom_cmd = build_hcom_command()
        print(f"'{instance_name}' exists but is {status} (no active session).")
        print(f"To rebind this session to '{instance_name}', run:")
        print(f"  {hcom_cmd} start --as {instance_name}")
        return 1

    # Active session binding exists, or ad-hoc CLI usage
    print(f"hcom already started for {instance_name}")
    return 0
