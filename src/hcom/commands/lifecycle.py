"""Lifecycle commands for HCOM instances"""
import os
import sys
import time
import random
import uuid
from pathlib import Path
from .utils import CLIError, format_error, is_interactive, resolve_identity
from ..shared import FG_YELLOW, RESET, IS_WINDOWS
from ..claude_args import resolve_claude_args, merge_claude_args, add_background_defaults, validate_conflicts
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
from ..core.runtime import build_claude_env
from ..hooks.utils import disable_instance


def cmd_launch(argv: list[str]) -> int:
    """Launch Claude instances: hcom [N] [claude] [args]"""
    # Import from terminal module
    from ..terminal import build_claude_command, launch_terminal

    try:
        # Parse arguments: hcom [N] [claude] [args]
        count = 1
        forwarded = []

        # Extract count if first arg is digit
        if argv and argv[0].isdigit():
            count = int(argv[0])
            if count <= 0:
                raise CLIError('Count must be positive.')
            if count > 100:
                raise CLIError('Too many instances requested (max 100).')
            argv = argv[1:]

        # Skip 'claude' keyword if present
        if argv and argv[0] == 'claude':
            argv = argv[1:]

        # Forward all remaining args to claude CLI
        forwarded = argv

        # Check for --no-auto-watch flag (used by TUI to prevent opening another watch window)
        no_auto_watch = '--no-auto-watch' in forwarded
        if no_auto_watch:
            forwarded = [arg for arg in forwarded if arg != '--no-auto-watch']

        # Get tag from config
        tag = get_config().tag
        if tag and '|' in tag:
            raise CLIError('Tag cannot contain "|" characters.')

        # Phase 1: Parse and merge Claude args (env + CLI with CLI precedence)
        env_spec = resolve_claude_args(None, get_config().claude_args)
        cli_spec = resolve_claude_args(forwarded if forwarded else None, None)

        # Merge: CLI overrides env on per-flag basis, inherits env if CLI has no args
        if cli_spec.clean_tokens or cli_spec.positional_tokens or cli_spec.system_entries:
            spec = merge_claude_args(env_spec, cli_spec)
        else:
            spec = env_spec

        # Validate parsed args
        if spec.has_errors():
            raise CLIError('\n'.join(spec.errors))

        # Check for conflicts (warnings only, not errors)
        warnings = validate_conflicts(spec)
        for warning in warnings:
            print(f"{FG_YELLOW}Warning:{RESET} {warning}", file=sys.stderr)

        # Add HCOM background mode enhancements
        spec = add_background_defaults(spec)

        # Extract values from spec
        background = spec.is_background
        # Use full tokens (prompts included) - respects user's HCOM_CLAUDE_ARGS config
        claude_args = spec.rebuild_tokens(include_system=True)

        terminal_mode = get_config().terminal

        # Fail fast for here mode with multiple instances
        if terminal_mode == 'here' and count > 1:
            print(format_error(
                f"'here' mode cannot launch {count} instances (it's one terminal window)",
                "Use 'hcom 1' for one instance"
            ), file=sys.stderr)
            return 1

        # Initialize database if needed
        from ..core.db import init_db
        init_db()

        # Check if launcher instance is enabled (for ready notification)
        launcher = resolve_identity().name
        launcher_data = load_instance_position(launcher)
        launcher_enabled = launcher_data.get('enabled', False) if launcher_data else False

        # Build environment variables for Claude instances
        base_env = build_claude_env()

        # Add tag-specific hints if provided
        if tag:
            base_env['HCOM_TAG'] = tag

        launched = 0

        # Generate batch ID for notification correlation (8 chars - first UUID segment)
        batch_id = str(uuid.uuid4()).split('-')[0]

        # Build claude command once (all args passed through to claude CLI)
        claude_cmd = build_claude_command(claude_args)

        # Launch count instances
        for _ in range(count):
            instance_env = base_env.copy()

            # Generate unique launch token for Windows identity
            launch_token = str(uuid.uuid4())
            instance_env['HCOM_LAUNCH_TOKEN'] = launch_token

            # Mark all hcom-launched instances with event ID
            instance_env['HCOM_LAUNCHED'] = '1'

            # Capture launch event ID for consistent message history start
            from ..core.db import get_last_event_id
            instance_env['HCOM_LAUNCH_EVENT_ID'] = str(get_last_event_id())

            # Track who launched this instance (use the one we already resolved)
            instance_env['HCOM_LAUNCHED_BY'] = launcher

            # Track batch for notification correlation
            instance_env['HCOM_LAUNCH_BATCH_ID'] = batch_id

            # Mark background instances via environment with log filename
            if background:
                # Generate unique log filename
                log_filename = f'background_{int(time.time())}_{random.randint(1000, 9999)}.log'
                instance_env['HCOM_BACKGROUND'] = log_filename

            try:
                if background:
                    log_file = launch_terminal(claude_cmd, instance_env, cwd=os.getcwd(), background=True)
                    if log_file:
                        print(f"Headless instance launched, log: {log_file}")
                        launched += 1
                else:
                    if launch_terminal(claude_cmd, instance_env, cwd=os.getcwd()):
                        launched += 1
            except Exception as e:
                print(format_error(f"Failed to launch terminal: {e}"), file=sys.stderr)

        requested = count
        failed = requested - launched

        if launched == 0:
            print(format_error(f"No instances launched (0/{requested})"), file=sys.stderr)
            return 1

        # Show results
        if failed > 0:
            print(f"Launched {launched}/{requested} Claude instance{'s' if requested != 1 else ''} ({failed} failed)")
        else:
            print(f"Launched {launched} Claude instance{'s' if launched != 1 else ''}")

        print(f"Batch id: {batch_id}")
        print(f"Check status of all launches + wait and notify when they're ready: hcom events launch")

        # Log launch event
        if launched > 0:
            try:
                from ..core.db import log_event
                launcher = resolve_identity().name
                log_event('life', launcher, {
                    'action': 'launched',
                    'by': launcher,
                    'batch_id': batch_id,
                    'count_requested': count,
                    'launched': launched,
                    'failed': failed,
                    'background': background,
                    'tag': tag or ''
                })
            except Exception:
                pass  # Don't break launch if logging fails

        # Auto-launch watch dashboard if in new window mode (new or custom) and all instances launched successfully
        terminal_mode = get_config().terminal

        # Only auto-watch if ALL instances launched successfully and launches windows (not 'here' or 'print') and not disabled by TUI
        if terminal_mode not in ('here', 'print') and failed == 0 and is_interactive() and not no_auto_watch:
            # Show tips first if needed
            if tag:
                print(f"\n  • Send to {tag} team: hcom send '@{tag} message'")

            # Clear transition message
            print("\nOpening hcom UI...")
            time.sleep(2)  # Brief pause so user sees the message

            # Launch interactive TUI (same as running bare `hcom`)
            from ..ui import run_tui  # Local import to avoid circular dependency
            return run_tui(hcom_path())
        else:
            tips = []
            if tag:
                tips.append(f"Send to {tag} team: hcom send '@{tag} message'")

            # Add ready detection tip
            if launched > 0:
                is_claude_code = os.environ.get('CLAUDECODE') == '1'
                if is_claude_code:
                    if launcher_enabled:
                        tips.append(f"You'll be automatically notified when all {launched} instances are launched & ready")
                    else:
                        tips.append(f"Run 'hcom start' to receive automatic notifications/messages from instances")
                else:
                    tips.append("Check status: hcom list")

            if tips:
                print("\n" + "\n".join(f"  • {tip}" for tip in tips) + "\n")

            return 0

    except ValueError as e:
        print(str(e), file=sys.stderr)
        return 1
    except Exception as e:
        print(str(e), file=sys.stderr)
        return 1


def cmd_stop(argv: list[str]) -> int:
    """Stop instances: hcom stop [alias|all]"""

    # Remove flags to get target
    args_without_flags = [a for a in argv if not a.startswith('--')]
    target = args_without_flags[0] if args_without_flags else None

    # Handle 'all' target
    if target == 'all':
        # Only stop local instances (not remote ones from other devices)
        instances = [i for i in iter_instances() if not i.get('origin_device_id')]

        if not instances:
            print("No instances found")
            return 0

        stopped_count = 0
        bg_logs = []
        stopped_names = []
        for instance_data in instances:
            if instance_data.get('enabled', False):
                instance_name = instance_data['name']
                launcher = resolve_identity().name
                disable_instance(instance_name, initiated_by=launcher, reason='stop_all')
                stopped_names.append(instance_name)
                stopped_count += 1

                # Track background logs
                if instance_data.get('background'):
                    log_file = instance_data.get('background_log_file', '')
                    if log_file:
                        bg_logs.append((instance_name, log_file))

        if stopped_count == 0:
            print("No instances to stop")
        else:
            print(f"Stopped {stopped_count} instance(s): {', '.join(stopped_names)}")

            # Show background logs if any
            if bg_logs:
                print()
                print("Headless instance logs:")
                for name, log_file in bg_logs:
                    print(f"  {name}: {log_file}")

        return 0

    # Resolve identity (target overrides automatic resolution)
    if target:
        instance_name = target
    else:
        try:
            identity = resolve_identity()
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
            raise CLIError("Cannot resolve instance identity - use 'hcom <n>' or Windows Terminal for stable identity")
        else:
            raise CLIError("Cannot resolve instance identity - launch via 'hcom <n>' for stable identity")

    # Error handling
    if not instance_name:
        raise CLIError("Cannot determine instance identity\nUsage: hcom stop <alias> | hcom stop all | prompt Claude to run 'hcom stop'")

    position = load_instance_position(instance_name)
    if not position:
        raise CLIError(f"Instance '{instance_name}' not found")

    # Remote instance - send control via relay
    if position.get('origin_device_id'):
        if ':' in instance_name:
            name, device_short_id = instance_name.rsplit(':', 1)
            from ..relay import send_control
            if send_control('stop', name, device_short_id):
                print(f"Stop sent to {instance_name}")
                return 0
            else:
                raise CLIError(f"Failed to send stop to {instance_name} - relay unavailable")
        raise CLIError(f"Cannot stop remote instance '{instance_name}' - missing device suffix")

    # Skip already stopped instances
    if not position.get('enabled', False):
        print(f"hcom already stopped for {instance_name}")
        return 0

    # Check if this is a subagent - disable only the targeted one
    if is_subagent_instance(position):
        # External stop = CLI user specified target, Self stop = no target (uses session_id)
        is_external_stop = target is not None
        launcher = resolve_identity().name
        reason = 'external' if is_external_stop else 'manual'
        disable_instance(instance_name, initiated_by=launcher, reason=reason)
        print(f"Stopped hcom for subagent {instance_name}. Will no longer receive chat messages automatically.")
    else:
        # Regular parent instance
        # External stop = CLI user specified target, Self stop = no target (uses session_id)
        is_external_stop = target is not None
        launcher = resolve_identity().name
        reason = 'external' if is_external_stop else 'manual'
        disable_instance(instance_name, initiated_by=launcher, reason=reason)
        print(f"Stopped hcom for {instance_name}. Will no longer receive chat messages automatically.")

    # Show background log location if applicable
    if position.get('background'):
        log_file = position.get('background_log_file', '')
        if log_file:
            print(f"\nHeadless instance log: {log_file}")

    return 0


def cmd_start(argv: list[str]) -> int:
    """Enable HCOM participation: hcom start [alias]"""
    from ..core.instances import initialize_instance_in_position_file, enable_instance, set_status

    # Extract --agentid flag (for subagents)
    agent_id = None

    # Parse subagent flag
    i = 0
    while i < len(argv):
        if argv[i] == '--agentid' and i + 1 < len(argv):
            agent_id = argv[i + 1]
            argv = argv[:i] + argv[i + 2:]
        else:
            i += 1

    # SUBAGENT PATH: --agentid provided (lazy creation)
    if agent_id:
        # Resolve parent from identity (session_id or mapid)
        try:
            parent_identity = resolve_identity()
            parent_name = parent_identity.name
        except ValueError:
            print("Error: Cannot resolve parent identity", file=sys.stderr)
            return 1

        parent_session_id = parent_identity.session_id

        # Validate parent has session_id (required for FK relationship)
        if parent_session_id is None:
            print("Error: Parent instance has no session_id (required for subagent creation)", file=sys.stderr)
            return 1

        # Look up agent_type from parent's running_tasks
        parent_data = load_instance_position(parent_name)
        if not parent_data:
            print("Error: Parent instance not found", file=sys.stderr)
            return 1

        running_tasks = parse_running_tasks(parent_data.get('running_tasks', ''))
        subagents = running_tasks.get('subagents', [])

        # Find agent_type for this agent_id
        agent_type = None
        for task in subagents:
            if task.get('agent_id') == agent_id:
                agent_type = task.get('type')
                break

        if not agent_type:
            print(f"Error: agent_id {agent_id} not found in parent's running_tasks.subagents", file=sys.stderr)
            return 1

        # Check if instance already exists by agent_id (reuse alias)
        from ..core.db import get_db
        import sqlite3
        import re
        conn = get_db()
        existing = conn.execute(
            "SELECT name FROM instances WHERE agent_id = ?",
            (agent_id,)
        ).fetchone()

        if existing:
            # Already created - reuse existing alias, re-enable if stopped
            alias = existing['name']
            instance_data = load_instance_position(alias)
            if instance_data and not instance_data.get('enabled', False):
                update_instance_position(alias, {'enabled': True})
                set_status(alias, 'active', 'start')
                print(f"hcom started for {alias}")
            else:
                print(f"hcom already started for {alias}")
            return 0

        # Compute next suffix: query max(n) for parent_type_% pattern
        pattern = f"{parent_name}_{agent_type}_%"
        rows = conn.execute(
            "SELECT name FROM instances WHERE name LIKE ?",
            (pattern,)
        ).fetchall()

        # Extract numeric suffixes and find max
        max_n = 0
        suffix_pattern = re.compile(rf'^{re.escape(parent_name)}_{re.escape(agent_type)}_(\d+)$')
        for row in rows:
            match = suffix_pattern.match(row['name'])
            if match:
                n = int(match.group(1))
                max_n = max(max_n, n)

        # Propose next alias
        alias = f"{parent_name}_{agent_type}_{max_n + 1}"

        # Single-pass insert with agent_id (direct DB insert, not via initialize_instance_in_position_file)
        import time
        from ..core.db import get_last_event_id
        initial_event_id = get_last_event_id() if SKIP_HISTORY else 0

        try:
            conn.execute(
                """INSERT INTO instances (name, session_id, parent_session_id, parent_name, agent_id, enabled, created_at, last_event_id, directory, last_stop)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                (alias, None, parent_session_id, parent_name, agent_id, 1, time.time(), initial_event_id, str(Path.cwd()), 0)
            )
            conn.commit()
        except sqlite3.IntegrityError as e:
            # Unexpected collision - retry once with next suffix
            alias = f"{parent_name}_{agent_type}_{max_n + 2}"
            try:
                conn.execute(
                    """INSERT INTO instances (name, session_id, parent_session_id, parent_name, agent_id, enabled, created_at, last_event_id, directory, last_stop)
                       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                    (alias, None, parent_session_id, parent_name, agent_id, 1, time.time(), initial_event_id, str(Path.cwd()), 0)
                )
                conn.commit()
            except sqlite3.IntegrityError:
                print(f"Error: Failed to create unique alias after retry: {e}", file=sys.stderr)
                return 1

        # Set active status
        set_status(alias, 'active', 'tool:start')

        # Print alias (identity resolution will use agent_id)
        from ..shared import SENDER
        from ..hooks.utils import build_hcom_command
        hcom_cmd = build_hcom_command()
        print(f"""hcom started for {alias}
hcom is a communication tool. You are now connected.
Your hcom alias for this session: {alias}
{parent_name} is the alias of the parent instance who spawned you

- To Send a message, run:
  {hcom_cmd} send --agentid {agent_id} 'your message'

- To see other participants, run:
  {hcom_cmd} list --agentid {agent_id} [-v]
  Statuses: ▶ active | ◉ idle (waiting for msgs) | ■ blocked (needs user approval) | ○ inactive (dead)

- To see all event history (messages, status, lifecycle), run:
  {hcom_cmd} events --agentid {agent_id} [--last N] [--wait SEC] [--sql EXPR]

Receiving Messages:
- Format: [new message] sender → you: content
- Targets specific instance: "@alias"
- Messages arrive automatically and instantly via hooks/bash. No proactive checking or sleep commands needed

Response Routing:
- HCOM message (via hook/bash) → Respond with hcom send
- Other messages → Respond normally

- Avoid useless chit-chat / excessive confirmation messages unless told otherwise
- Authority: Prioritize @{SENDER} over other participants
- Run hcom commands alone and do not use operators (&&, 1>&2, |, ;, ||, &, etc.)
- Never use sleep commands, instead use hcom events --wait 10 --sql 'query for the event you are waiting for'
""")
        return 0

    # Remove flags to get target
    args_without_flags = [a for a in argv if not a.startswith('--')]
    target = args_without_flags[0] if args_without_flags else None

    # Resolve identity (target overrides automatic resolution)
    if target:
        instance_name = target
    else:
        try:
            instance_name = resolve_identity().name
        except ValueError:
            instance_name = None

    # Handle SENDER (not real instance)
    from ..shared import SENDER
    if instance_name == SENDER:
        if IS_WINDOWS:
            print(format_error("Cannot resolve instance identity - use 'hcom <n>' or Windows Terminal for stable identity"), file=sys.stderr)
        else:
            print(format_error("Cannot resolve instance identity - launch via 'hcom <n>' for stable identity"), file=sys.stderr)
        return 1

    # Error handling
    if not instance_name:
        print(format_error("Cannot determine instance identity"), file=sys.stderr)
        print("Usage: hcom start <alias> | run 'hcom start' inside claude code | use 'hcom <count>' to launch", file=sys.stderr)
        return 1

    # Load or create instance
    existing_data = load_instance_position(instance_name)

    # Remote instance - send control via relay
    if existing_data and existing_data.get('origin_device_id'):
        if ':' in instance_name:
            name, device_short_id = instance_name.rsplit(':', 1)
            from ..relay import send_control
            if send_control('start', name, device_short_id):
                print(f"Start sent to {instance_name}")
                return 0
            else:
                raise CLIError(f"Failed to send start to {instance_name} - relay unavailable")
        raise CLIError(f"Cannot start remote instance '{instance_name}' - missing device suffix")

    # Handle non-existent instance
    if not existing_data:
        # Explicit target provided → must already exist (re-enable only)
        if target:
            print(format_error(f"Instance '{instance_name}' not found"), file=sys.stderr)
            print("Usage: hcom start <alias>   Re-enable existing stopped instance", file=sys.stderr)
            print("       hcom start           Enable hcom (inside Claude Code)", file=sys.stderr)
            print("       hcom <count>         Launch new instances", file=sys.stderr)
            return 1

        # Self-start (no target) → create new instance for current session
        from ..shared import MAPID
        session_id = os.environ.get('HCOM_SESSION_ID')

        # Windows fallback: Look up session_id via MAPID mapping
        if not session_id and MAPID:
            from ..core.db import get_db
            conn = get_db()
            row = conn.execute(
                "SELECT session_id FROM mapid_sessions WHERE mapid = ?",
                (MAPID,)
            ).fetchone()
            session_id = row['session_id'] if row else None

        # Pass both session_id (Unix) and MAPID (Windows) for cross-platform identity
        initialize_instance_in_position_file(instance_name, session_id, mapid=MAPID)
        launcher = resolve_identity().name
        enable_instance(instance_name, initiated_by=launcher, reason='manual')
        print(f"\nStarted hcom for {instance_name}")
        return 0

    # Skip already started instances
    if existing_data.get('enabled', False):
        print(f"hcom already started for {instance_name}")
        return 0

    # Check if background instance has exited permanently
    if existing_data.get('session_ended') and existing_data.get('background'):
        session = existing_data.get('session_id', '')
        msg = f"Cannot start hcom for {instance_name}: headless instance has exited permanently\n"
        if session:
            msg += f"\nResume conversation with same hcom identity: hcom 1 claude -p --resume {session}"
        raise CLIError(msg)

    # Re-enabling existing instance
    launcher = resolve_identity().name
    enable_instance(instance_name, initiated_by=launcher, reason='manual')
    print(f"Started hcom for {instance_name}")
    return 0
