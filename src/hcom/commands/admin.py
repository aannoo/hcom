"""Admin commands for HCOM"""
import sys
import json
import time
import shutil
from pathlib import Path
from datetime import datetime
from typing import Any
from .utils import get_help_text, format_error
from ..core.paths import hcom_path, SCRIPTS_DIR, LOGS_DIR, ARCHIVE_DIR
from ..core.instances import get_instance_status, is_external_sender
from ..shared import STATUS_ICONS, format_age, shorten_path


def get_archive_timestamp() -> str:
    """Get timestamp for archive files"""
    return datetime.now().strftime("%Y-%m-%d_%H%M%S")




def cmd_help() -> int:
    """Show help text"""
    print(get_help_text())
    return 0


def _cmd_events_launch(argv: list[str]) -> int:
    """Wait for launches ready, output JSON. Internal - called by launch output."""
    from ..core.db import get_launch_status, get_launch_batch, init_db
    from .utils import resolve_identity
    import os

    init_db()

    # Parse batch_id arg (for specific batch lookup)
    batch_id = argv[0] if argv and not argv[0].startswith('--') else None

    # Find launcher identity if in Claude context
    launcher = None
    if os.environ.get('CLAUDECODE') == '1':
        try:
            launcher = resolve_identity().name
        except Exception:
            pass

    # Get status - specific batch or aggregated
    if batch_id:
        status_data = get_launch_batch(batch_id)
    else:
        status_data = get_launch_status(launcher)

    if not status_data:
        msg = "You haven't launched any instances" if launcher else "No launches found"
        print(json.dumps({"status": "no_launches", "message": msg}))
        return 0

    # Wait up to 30s for all instances to be ready
    start_time = time.time()
    while status_data['ready'] < status_data['expected'] and time.time() - start_time < 30:
        time.sleep(0.5)
        if batch_id:
            status_data = get_launch_batch(batch_id)
        else:
            status_data = get_launch_status(launcher)
        if not status_data:
            # DB reset or batch pruned mid-wait
            print(json.dumps({"status": "error", "message": "Launch data disappeared (DB reset or pruned)"}))
            return 1

    # Output JSON
    is_timeout = status_data['ready'] < status_data['expected']
    status = "timeout" if is_timeout else "ready"
    result = {
        "status": status,
        "expected": status_data['expected'],
        "ready": status_data['ready'],
        "instances": status_data['instances'],
        "launcher": status_data['launcher'],
        "timestamp": status_data['timestamp'],
    }
    # Include batches list if aggregated
    if 'batches' in status_data:
        result["batches"] = status_data['batches']
    else:
        result["batch_id"] = status_data.get('batch_id')

    if is_timeout:
        result["timed_out"] = True
        # Identify which batch(es) failed
        batch_info = result.get('batch_id') or (result.get('batches', ['?'])[0] if result.get('batches') else '?')
        result["hint"] = f"Launch failed: {status_data['ready']}/{status_data['expected']} ready after 30s (batch: {batch_info}). Check ~/.hcom/.tmp/logs/background_*.log or hcom list -v"
    print(json.dumps(result))

    return 0 if status == "ready" else 1


def cmd_events(argv: list[str]) -> int:
    """Query events from SQLite: hcom events [launch] [--last N] [--wait SEC] [--sql EXPR] [--agentid ID]"""
    from ..core.db import get_db, init_db, get_last_event_id
    from .utils import parse_agentid_flag

    init_db()  # Ensure schema exists

    # Handle 'launch' subcommand
    if argv and argv[0] == 'launch':
        return _cmd_events_launch(argv[1:])

    # Parse --agentid flag (for subagents)
    subagent_id, argv, agent_id_value = parse_agentid_flag(argv)

    # Check if --agentid was provided but instance not found
    if subagent_id is None and agent_id_value is not None:
        print(format_error(f"No instance found with agent_id '{agent_id_value}'"), file=sys.stderr)
        print(f"Run 'hcom start --agentid {agent_id_value}' first", file=sys.stderr)
        return 1

    # Parse arguments
    last_n = 20  # Default: last 20 events
    wait_timeout = None
    sql_where = None

    i = 0
    while i < len(argv):
        if argv[i] == '--last' and i + 1 < len(argv):
            try:
                last_n = int(argv[i + 1])
            except ValueError:
                print(f"Error: --last must be an integer, got '{argv[i + 1]}'", file=sys.stderr)
                return 1
            i += 2
        elif argv[i] == '--wait':
            if i + 1 < len(argv) and not argv[i + 1].startswith('--'):
                try:
                    wait_timeout = int(argv[i + 1])
                except ValueError:
                    print(f"Error: --wait must be an integer, got '{argv[i + 1]}'", file=sys.stderr)
                    return 1
                i += 2
            else:
                wait_timeout = 60  # Default: 60 seconds
                i += 1
        elif argv[i] == '--sql' and i + 1 < len(argv):
            sql_where = argv[i + 1]
            i += 2
        else:
            i += 1

    # Build base query for filters
    db = get_db()
    filter_query = ""

    # Add user SQL WHERE clause directly (no validation needed)
    # Note: SQL injection is not a security concern in hcom's threat model.
    # User (or ai) owns ~/.hcom/hcom.db and can already run: sqlite3 ~/.hcom/hcom.db "anything"
    # Validation would block legitimate queries while providing no actual security.
    if sql_where:
        filter_query += f" AND ({sql_where})"

    # Wait mode: block until matching event or timeout
    if wait_timeout:
        # Check for matching events in last 10s (race condition window)
        from datetime import timezone
        lookback_timestamp = datetime.fromtimestamp(time.time() - 10, tz=timezone.utc).isoformat()
        lookback_query = f"SELECT * FROM events WHERE timestamp > ?{filter_query} ORDER BY id DESC LIMIT 1"

        try:
            lookback_row = db.execute(lookback_query, [lookback_timestamp]).fetchone()
        except Exception as e:
            print(f"Error in SQL WHERE clause: {e}", file=sys.stderr)
            return 2

        if lookback_row:
            try:
                event = {
                    'ts': lookback_row['timestamp'],
                    'type': lookback_row['type'],
                    'instance': lookback_row['instance'],
                    'data': json.loads(lookback_row['data'])
                }
                # Found recent matching event, return immediately
                print(json.dumps(event))
                return 0
            except (json.JSONDecodeError, TypeError):
                pass  # Ignore corrupt event, continue to wait loop

        start_time = time.time()
        last_id = get_last_event_id()

        while time.time() - start_time < wait_timeout:
            query = f"SELECT * FROM events WHERE id > ?{filter_query} ORDER BY id"

            try:
                rows = db.execute(query, [last_id]).fetchall()
            except Exception as e:
                print(f"Error in SQL WHERE clause: {e}", file=sys.stderr)
                return 2

            if rows:
                # Process matching events
                for row in rows:
                    try:
                        event = {
                            'ts': row['timestamp'],
                            'type': row['type'],
                            'instance': row['instance'],
                            'data': json.loads(row['data'])
                        }

                        # Event matches all conditions, print and exit
                        print(json.dumps(event))
                        return 0

                    except (json.JSONDecodeError, TypeError) as e:
                        # Skip corrupt events, log to stderr
                        print(f"Warning: Skipping corrupt event ID {row['id']}: {e}", file=sys.stderr)
                        continue

                # All events processed, update last_id and continue waiting
                last_id = rows[-1]['id']

            # Check if current instance received @mention (interrupt wait)
            from .utils import resolve_identity
            from ..core.messages import get_unread_messages

            identity = resolve_identity(subagent_id=subagent_id)
            if identity.kind == 'instance':
                messages, _ = get_unread_messages(identity.name, update_position=False)
                if messages:
                    # Interrupted by @mention - exit 0 so PostToolUse runs and delivers
                    return 0

            # Sync remote events (long-poll if backend available)
            from ..relay import relay_wait
            remaining = wait_timeout - (time.time() - start_time)
            if remaining > 0:
                if not relay_wait(min(remaining, 25)):
                    time.sleep(0.5)  # Fallback when no backend

        print("timeout")  # Let Claude know wait expired without match
        return 0  # Always exit 0 so PostToolUse runs for message delivery

    # Snapshot mode (default)
    query = "SELECT * FROM events WHERE 1=1"
    query += filter_query
    query += " ORDER BY id DESC"
    query += f" LIMIT {last_n}"

    try:
        rows = db.execute(query).fetchall()
    except Exception as e:
        print(f"Error in SQL WHERE clause: {e}", file=sys.stderr)
        return 2
    # Reverse to chronological order
    for row in reversed(rows):
        try:
            event = {
                'ts': row['timestamp'],
                'type': row['type'],
                'instance': row['instance'],
                'data': json.loads(row['data'])
            }
            print(json.dumps(event))
        except (json.JSONDecodeError, TypeError) as e:
            # Skip corrupt events, log to stderr
            print(f"Warning: Skipping corrupt event ID {row['id']}: {e}", file=sys.stderr)
            continue
    return 0


def cmd_list(argv: list[str]) -> int:
    """List instances: hcom list [--json] [-v|--verbose] [--agentid ID]"""
    from .utils import resolve_identity, parse_agentid_flag
    from ..core.instances import load_instance_position, set_status
    from ..core.messages import get_read_receipts
    from ..core.db import get_db
    from ..shared import SENDER

    # Parse --agentid flag (for subagents)
    subagent_id, argv, agent_id_value = parse_agentid_flag(argv)

    # Check if --agentid was provided but instance not found
    if subagent_id is None and agent_id_value is not None:
        print(format_error(f"No instance found with agent_id '{agent_id_value}'"), file=sys.stderr)
        print(f"Run 'hcom start --agentid {agent_id_value}' first", file=sys.stderr)
        return 1

    # Parse arguments
    json_output = False
    verbose_output = False

    for arg in argv:
        if arg == '--json':
            json_output = True
        elif arg in ['-v', '--verbose']:
            verbose_output = True

    # Set status for subagents
    if subagent_id:
        set_status(subagent_id, 'active', 'tool:list')

    # Resolve current instance identity (with subagent context)
    identity = resolve_identity(subagent_id=subagent_id)
    current_alias = identity.name
    sender_identity = identity

    # Load read receipts for all contexts (bigboss, instances)
    # JSON output gets all receipts; verbose gets 3; default gets 1
    read_limit = None if json_output else (3 if verbose_output else 1)
    read_receipts = get_read_receipts(sender_identity, limit=read_limit)

    # Only show connection status for actual instances (not CLI/fallback)
    show_connection = current_alias != SENDER
    current_enabled = False
    if show_connection:
        current_data = load_instance_position(current_alias)
        current_enabled = current_data.get('enabled', False) if current_data else False

    # Query instances (default: enabled only, -v or --json: all)
    db = get_db()
    if json_output or verbose_output:
        query = "SELECT * FROM instances ORDER BY created_at DESC"
        rows = db.execute(query).fetchall()
    else:
        query = "SELECT * FROM instances WHERE enabled = 1 ORDER BY created_at DESC"
        rows = db.execute(query).fetchall()

    # Also count stopped instances for the summary
    stopped_count = db.execute("SELECT COUNT(*) FROM instances WHERE enabled = 0").fetchone()[0]

    # Convert rows to dictionaries
    sorted_instances = [dict(row) for row in rows]

    if json_output:
        # JSON per line - _self entry first always
        self_payload = {
            "_self": {
                "alias": current_alias,
                "read_receipts": read_receipts
            }
        }
        if verbose_output and identity.session_id:
            self_payload["_self"]["session_id"] = identity.session_id
        # Only include connection status for actual instances
        if show_connection:
            self_payload["_self"]["hcom_connected"] = current_enabled
        print(json.dumps(self_payload))

        for data in sorted_instances:
            name = data['name']
            enabled, status, age_str, description, age_seconds = get_instance_status(data)
            payload = {
                name: {
                    "hcom_connected": enabled,
                    "status": status,
                    "status_age_seconds": int(age_seconds),
                    "description": description,
                    "headless": bool(data.get("background", False)),
                    "wait_timeout": data.get("wait_timeout", 1800),
                    "session_id": data.get("session_id", ""),
                    "directory": data.get("directory", ""),
                    "parent_name": data.get("parent_name") or None,
                    "agent_id": data.get("agent_id") or None,
                    "background_log_file": data.get("background_log_file") or None,
                    "transcript_path": data.get("transcript_path") or None,
                    "created_at": data.get("created_at"),
                    "tcp_mode": bool(data.get("tcp_mode", False)),
                }
            }
            print(json.dumps(payload))
    else:
        # Human-readable - show header with alias and read receipts
        print(f"Your alias: {current_alias}")

        # Show connection status only for actual instances (not bigboss)
        if show_connection:
            state_symbol = "+" if current_enabled else "-"
            state_text = "enabled" if current_enabled else "disabled"
            print(f"  Your hcom connection: {state_text} ({state_symbol})")

        # Show read receipts if any
        if read_receipts:
            print(f"  Read receipts:")
            for msg in read_receipts:
                read_count = len(msg['read_by'])
                total = msg['total_recipients']

                if verbose_output:
                    # Verbose: show list of who has read + ratio
                    readers = ", ".join(msg['read_by']) if msg['read_by'] else "(none)"
                    print(f"    #{msg['id']} {msg['age']} \"{msg['text']}\" | read by ({read_count}/{total}): {readers}")
                else:
                    # Default: just show ratio
                    print(f"    #{msg['id']} {msg['age']} \"{msg['text']}\" | read by {read_count}/{total}")

        print()

        for data in sorted_instances:
            name = data['name']
            enabled, status, age_str, description, age_seconds = get_instance_status(data)
            icon = STATUS_ICONS.get(status, '◦')
            state = "+" if enabled else "-"
            age_display = f"{age_str} ago" if age_str else ""
            desc_sep = ": " if description else ""

            # Add badges
            from ..core.instances import is_remote_instance
            headless_badge = "[headless]" if data.get("background", False) else ""
            external_badge = "[external]" if is_external_sender(data) else ""
            remote_badge = "[remote]" if is_remote_instance(data) else ""
            badge_parts = [b for b in [headless_badge, external_badge, remote_badge] if b]
            badge_str = (" " + " ".join(badge_parts)) if badge_parts else ""
            name_with_badges = f"{name}{badge_str}"

            # Main status line
            print(f"{icon} {name_with_badges:30} {state}  {age_display}{desc_sep}{description}")

            if verbose_output:
                # Multi-line detailed view
                import os
                from ..core.instances import is_remote_instance

                if is_remote_instance(data):
                    # Remote instance: show device info plus available details
                    origin_device = data.get("origin_device_id", "")
                    device_short = origin_device[:8] if origin_device else "(unknown)"

                    # Get device sync time from kv store
                    from ..core.db import kv_get
                    sync_time = 0
                    try:
                        ts = kv_get(f'relay_sync_time_{origin_device}')
                        if ts:
                            sync_time = float(ts)
                    except Exception:
                        pass

                    sync_age = _format_time(sync_time) if sync_time else "never"

                    print(f"    device:       {device_short}")
                    print(f"    last_sync:    {sync_age}")

                    session_id = data.get("session_id", "(none)")
                    print(f"    session_id:   {session_id}")

                    parent = data.get("parent_name")
                    if parent:
                        print(f"    parent:       {parent}")

                    directory = data.get("directory")
                    if directory:
                        print(f"    directory:    {shorten_path(directory)}")

                    status_time = data.get("status_time", 0)
                    if status_time:
                        print(f"    status_time:  {_format_time(status_time)}")

                    print()
                else:
                    # Local instance: show full details
                    session_id = data.get("session_id", "(none)")
                    directory = data.get("directory", "(none)")
                    timeout = data.get("wait_timeout", 1800)

                    parent = data.get("parent_name") or "(none)"

                    # Format paths (shorten with ~)
                    log_file = shorten_path(data.get("background_log_file")) or "(none)"
                    transcript = shorten_path(data.get("transcript_path")) or "(none)"

                    # Format created_at timestamp
                    created_ts = data.get("created_at")
                    created = f"{format_age(time.time() - created_ts)} ago" if created_ts else "(unknown)"

                    # Format tcp_mode
                    tcp = "TCP" if data.get("tcp_mode") else "polling"

                    # Get subagent agentId if this is a subagent
                    agent_id = None
                    if parent != "(none)":
                        agent_id = data.get("agent_id") or "(none)"

                    # Print indented details
                    print(f"    session_id:   {session_id}")
                    print(f"    created:      {created}")
                    print(f"    directory:    {directory}")
                    print(f"    timeout:      {timeout}s")
                    if parent != "(none)":
                        print(f"    parent:       {parent}")
                        print(f"    agent_id:     {agent_id}")
                    print(f"    tcp_mode:     {tcp}")
                    if log_file != "(none)":
                        print(f"    headless log: {log_file}")
                    print(f"    transcript:   {transcript}")
                    print()  # Blank line between instances

        # Show stopped count if any are hidden (default view hides stopped)
        if not verbose_output and stopped_count > 0:
            print(f"\n({stopped_count} stopped instance{'s' if stopped_count != 1 else ''} hidden, use -v to show all)")

    return 0


def clear() -> int:
    """Clear and archive conversation"""
    from ..core.db import DB_FILE, close_db, get_db

    db_file = hcom_path(DB_FILE)
    db_wal = hcom_path(f'{DB_FILE}-wal')
    db_shm = hcom_path(f'{DB_FILE}-shm')

    # cleanup: temp files, old scripts, old background logs
    cutoff_time_24h = time.time() - (24 * 60 * 60)  # 24 hours ago
    cutoff_time_30d = time.time() - (30 * 24 * 60 * 60)  # 30 days ago

    scripts_dir = hcom_path(SCRIPTS_DIR)
    if scripts_dir.exists():
        for f in scripts_dir.glob('*'):
            if f.is_file() and f.stat().st_mtime < cutoff_time_24h:
                f.unlink(missing_ok=True)

    # Rotate hooks.log at 1MB
    logs_dir = hcom_path(LOGS_DIR)
    hooks_log = logs_dir / 'hooks.log'
    if hooks_log.exists() and hooks_log.stat().st_size > 1_000_000:  # 1MB
        archive_logs = logs_dir / f'hooks.log.{get_archive_timestamp()}'
        hooks_log.rename(archive_logs)

    # Clean background logs older than 30 days
    if logs_dir.exists():
        for f in logs_dir.glob('background_*.log'):
            if f.stat().st_mtime < cutoff_time_30d:
                f.unlink(missing_ok=True)

    # Check if DB exists
    if not db_file.exists():
        print("No HCOM conversation to clear")
        return 0

    # Archive database if it has content
    timestamp = get_archive_timestamp()
    archived = False

    try:
        # Check if DB has content
        db = get_db()
        event_count = db.execute("SELECT COUNT(*) FROM events").fetchone()[0]
        instance_count = db.execute("SELECT COUNT(*) FROM instances").fetchone()[0]

        if event_count > 0 or instance_count > 0:
            # Create session archive folder with timestamp
            session_archive = hcom_path(ARCHIVE_DIR, f'session-{timestamp}')
            session_archive.mkdir(parents=True, exist_ok=True)

            # Checkpoint WAL before archiving (attempts to consolidate WAL into main DB)
            # Using PASSIVE mode - doesn't force if writers active
            db.execute("PRAGMA wal_checkpoint(PASSIVE)")
            db.commit()
            close_db()

            # Copy all DB files to archive (DB + WAL + SHM)
            # This preserves WAL data in case checkpoint was incomplete
            # SQLite can recover from WAL when opening archived DB
            shutil.copy2(db_file, session_archive / DB_FILE)
            if db_wal.exists():
                shutil.copy2(db_wal, session_archive / f'{DB_FILE}-wal')
            if db_shm.exists():
                shutil.copy2(db_shm, session_archive / f'{DB_FILE}-shm')

            # Delete main DB and WAL/SHM files
            db_file.unlink()
            db_wal.unlink(missing_ok=True)
            db_shm.unlink(missing_ok=True)

            archived = True
        else:
            # Empty DB, just delete
            close_db()
            db_file.unlink()
            db_wal.unlink(missing_ok=True)
            db_shm.unlink(missing_ok=True)

        if archived:
            print(f"Archived to archive/session-{timestamp}/")

        print("Started fresh HCOM conversation")
        return 0

    except Exception as e:
        print(format_error(f"Failed to archive: {e}"), file=sys.stderr)
        return 1


def remove_global_hooks() -> bool:
    """Remove HCOM hooks from ~/.claude/settings.json"""
    from ..hooks.settings import get_claude_settings_path, load_settings_json, _remove_hcom_hooks_from_settings
    from ..core.paths import atomic_write

    settings_path = get_claude_settings_path()

    if not settings_path.exists():
        return True

    try:
        settings = load_settings_json(settings_path, default=None)
        if not settings:
            return False

        _remove_hcom_hooks_from_settings(settings)
        atomic_write(settings_path, json.dumps(settings, indent=2))
        return True
    except Exception:
        return False


def reset_config() -> int:
    """Archive and reset config to defaults. Returns exit code."""
    from ..core.paths import CONFIG_FILE

    config_path = hcom_path(CONFIG_FILE)
    if config_path.exists():
        timestamp = datetime.now().strftime('%Y%m%d_%H%M%S')
        archive_config_dir = hcom_path(ARCHIVE_DIR, 'config')
        archive_config_dir.mkdir(parents=True, exist_ok=True)
        archive_path = archive_config_dir / f'config.env.{timestamp}'
        shutil.copy2(config_path, archive_path)
        config_path.unlink()
        print(f"Config archived to archive/config/config.env.{timestamp}")
        return 0
    else:
        print("No config file to reset")
        return 0


def cmd_reset(argv: list[str]) -> int:
    """Reset HCOM components.

    Usage:
        hcom reset          Clear database (archive conversation)
        hcom reset hooks    Remove hooks only
        hcom reset all      Stop all + clear db + remove hooks + reset config
    """
    from .lifecycle import cmd_stop

    # Parse subcommand
    target = argv[0] if argv else None

    # Validate
    if target and target not in ('hooks', 'all'):
        from .utils import get_command_help
        print(f"Unknown target: {target}\n", file=sys.stderr)
        print(get_command_help('reset'), file=sys.stderr)
        return 1

    if len(argv) > 1:
        from .utils import get_command_help
        print(f"Too many arguments\n", file=sys.stderr)
        print(get_command_help('reset'), file=sys.stderr)
        return 1

    exit_codes = []

    # hooks: just remove hooks and exit
    if target == 'hooks':
        if remove_global_hooks():
            print("Removed hooks")
            return 0
        else:
            print("Warning: Could not remove hooks", file=sys.stderr)
            return 1

    # all: stop all instances first
    if target == 'all':
        exit_codes.append(cmd_stop(['all']))

    # Clear database
    exit_codes.append(clear())

    # Log reset event (used for local import filtering + relay to other devices)
    from ..core.db import log_event, kv_set
    from ..core.device import get_device_uuid
    import time as time_module
    reset_ts = time_module.time()
    log_event(
        event_type='life',
        instance='_device',
        data={'action': 'reset', 'device': get_device_uuid()}
    )
    # Persist reset timestamp in KV for reliable cross-process access
    kv_set('relay_local_reset_ts', str(reset_ts))

    # Push reset event to relay server
    try:
        from ..relay import push
        push(force=True)
    except Exception:
        pass  # Best effort

    # Pull fresh state from other devices
    try:
        from ..relay import pull
        pull()
    except Exception as e:
        print(f"Warning: Failed to pull remote state: {e}", file=sys.stderr)

    # all: also remove hooks and reset config
    if target == 'all':
        if remove_global_hooks():
            print("Removed hooks")
        else:
            print("Warning: Could not remove hooks", file=sys.stderr)
            exit_codes.append(1)

        exit_codes.append(reset_config())

    return max(exit_codes) if exit_codes else 0


def cmd_relay(argv: list[str]) -> int:
    """Relay management: hcom relay [on|off|pull|poll|hf]

    Usage:
        hcom relay                Show relay status
        hcom relay on             Enable relay sync
        hcom relay off            Disable relay sync
        hcom relay pull           Manual sync (pull + push)
        hcom relay poll [sec]     Long-poll for changes
        hcom relay hf [token]     Setup HuggingFace Space relay
        hcom relay hf --update    Update relay to latest version
    """
    if not argv:
        return _relay_status()
    elif argv[0] == 'on':
        return _relay_toggle(True)
    elif argv[0] == 'off':
        return _relay_toggle(False)
    elif argv[0] == 'pull':
        return _relay_pull()
    elif argv[0] == 'poll':
        return _relay_poll(argv[1:])
    elif argv[0] == 'hf':
        return _relay_hf(argv[1:])
    else:
        from .utils import get_command_help
        print(f"Unknown subcommand: {argv[0]}\n", file=sys.stderr)
        print(get_command_help('relay'), file=sys.stderr)
        return 1


def _relay_toggle(enable: bool) -> int:
    """Enable or disable relay sync."""
    from ..core.config import load_config_snapshot, save_config_snapshot, get_config, reload_config

    config = get_config()

    # Check if relay URL is configured
    if not config.relay:
        print("No relay URL configured.", file=sys.stderr)
        print("Run: hcom relay hf <token>", file=sys.stderr)
        return 1

    # Update config
    snapshot = load_config_snapshot()
    snapshot.core.relay_enabled = enable
    save_config_snapshot(snapshot)
    reload_config()  # Invalidate cache so push/pull see new relay_enabled value

    if enable:
        print("Relay enabled\n")
        return _relay_status()
    else:
        print("Relay: disabled")
        print(f"URL still configured: {config.relay}")
        print("\nRun 'hcom relay on' to reconnect")

    return 0


def _relay_status() -> int:
    """Show relay status and configuration"""
    import urllib.request
    from ..core.device import get_device_short_id
    from ..core.config import get_config
    from ..core.db import kv_get, get_last_event_id
    from ..relay import push, pull

    config = get_config()

    if not config.relay:
        print("Relay: not configured")
        print("Run: hcom relay hf <token>")
        return 0

    if not config.relay_enabled:
        print("Relay: disabled (URL configured)")
        print(f"URL: {config.relay}")
        print("\nRun: hcom relay on")
        return 0

    exit_code = 0

    # Push first (heartbeat - so this device shows as active)
    push(force=True)

    # Pull to catch up immediately
    _, pull_err = pull()
    if pull_err:
        print(f"Pull failed: {pull_err}", file=sys.stderr)
        exit_code = 1

    # Ping relay to check if online
    relay_online = False
    relay_version = None
    headers = {'Authorization': f'Bearer {config.relay_token}'} if config.relay_token else {}
    try:
        url = config.relay.rstrip('/') + '/version'
        req = urllib.request.Request(url, headers=headers)
        with urllib.request.urlopen(req, timeout=5) as r:
            relay_version = json.loads(r.read()).get('v')
            relay_online = True
    except Exception:
        pass

    # Version warning first (if outdated)
    if relay_version is not None and relay_version != REQUIRED_RELAY_VERSION:
        print(f"⚠ Relay server outdated (v{relay_version}). Run: hcom relay hf --update\n")

    # Server status
    if relay_online:
        print("Status: online")
    else:
        print("Status: OFFLINE")
        print(f"URL: {config.relay}")
        print("\n⚠ Cannot reach relay server. Check URL or wait for Space to start.")
        return 1

    print(f"URL: {config.relay}")
    print(f"Device ID: {get_device_short_id()}")

    # Queued events (local only - remote events have : in instance name)
    from ..core.db import get_db
    conn = get_db()
    last_push_id = int(kv_get('relay_last_push_id') or 0)
    queued = conn.execute(
        "SELECT COUNT(*) FROM events WHERE id > ? AND instance NOT LIKE '%:%'",
        (last_push_id,)
    ).fetchone()[0]
    print(f"Queued: {queued} events pending" if queued > 0 else "Queued: up to date")

    # Last push
    last_push = float(kv_get('relay_last_push') or 0)
    print(f"Last push: {_format_time(last_push)}" if last_push else "Last push: never")

    # Live remote devices from server
    try:
        devices_url = config.relay.rstrip('/') + '/devices'
        req = urllib.request.Request(devices_url, headers=headers)
        with urllib.request.urlopen(req, timeout=5) as r:
            remote_devices = json.loads(r.read())

        my_short_id = get_device_short_id()
        active = [d for d in remote_devices if d.get('age', 9999) < 300 and d.get('short_id') != my_short_id]

        if active:
            print("\nActive devices:")
            for d in active:
                print(f"  {d['short_id']}: {d['instances']} instances ({format_age(d['age'])} ago)")
        else:
            print("\nNo other active devices")
    except Exception:
        print("\nNo other active devices")

    return exit_code


def _format_time(timestamp: float) -> str:
    """Format timestamp for display (wrapper around format_age)"""
    if not timestamp:
        return "never"
    return f"{format_age(time.time() - timestamp)} ago"


REQUIRED_RELAY_VERSION = 1  # Bump when hcom needs new relay features


def _check_relay_version() -> tuple[bool, str | None]:
    """Check if relay version matches required version."""
    import urllib.request
    from ..core.config import get_config

    config = get_config()
    if not config.relay:
        return (False, None)

    try:
        url = config.relay.rstrip('/') + '/version'
        headers = {'Authorization': f'Bearer {config.relay_token}'} if config.relay_token else {}
        req = urllib.request.Request(url, headers=headers)
        with urllib.request.urlopen(req, timeout=5) as r:
            relay_version = json.loads(r.read()).get('v', 0)

        if relay_version != REQUIRED_RELAY_VERSION:
            return (True, f"Relay v{relay_version}, need v{REQUIRED_RELAY_VERSION}. Run: hcom relay hf --update")
        return (False, None)
    except Exception:
        return (False, None)  # Fail silently (relay might not have /version yet (it will, this is shit))


def _relay_pull() -> int:
    """Manual sync trigger (pull + push)"""
    from ..relay import push, pull

    # Check for outdated relay
    outdated, msg = _check_relay_version()
    if outdated:
        print(f"⚠ {msg}", file=sys.stderr)

    ok, push_err = push(force=True)
    if push_err:
        print(f"Push failed: {push_err}", file=sys.stderr)

    result, pull_err = pull()
    if pull_err:
        print(f"Pull failed: {pull_err}", file=sys.stderr)
        return 1

    devices = result.get('devices', {})
    print(f"Synced with {len(devices)} remote devices")
    return 0


def _relay_poll(argv: list[str]) -> int:
    """Long-poll for changes, exit when data arrives or timeout.

    Used by TUI subprocess for efficient cross-device sync.
    Returns 0 if new data arrived, 1 on timeout.
    """
    from ..relay import relay_wait

    timeout = 55
    if argv and argv[0].isdigit():
        timeout = int(argv[0])

    start_time = time.time()
    while time.time() - start_time < timeout:
        remaining = timeout - (time.time() - start_time)
        if remaining <= 0:
            break
        if relay_wait(min(remaining, 25)):
            return 0  # New data arrived
        time.sleep(1)  # Brief backoff
    return 1  # Timeout, no data


def _relay_hf(argv: list[str]) -> int:
    """Setup HF Space relay"""
    import urllib.request
    import urllib.error
    import os
    from pathlib import Path
    from ..core.config import load_config_snapshot, save_config_snapshot

    SOURCE_SPACE = "aannoo/hcom-relay"

    def get_hf_token() -> str | None:
        """Get HF token from env or cached file."""
        if token := os.getenv('HF_TOKEN'):
            return token
        hf_home = Path(os.getenv('HF_HOME', Path.home() / '.cache' / 'huggingface'))
        token_path = hf_home / 'token'
        if token_path.exists():
            return token_path.read_text().strip()
        return None

    # Parse args: [token] [--name NAME] [--update]
    token = None
    space_name = "hcom-relay"
    do_update = False
    i = 0
    while i < len(argv):
        if argv[i] == '--name' and i + 1 < len(argv):
            space_name = argv[i + 1]
            i += 2
        elif argv[i] == '--update':
            do_update = True
            i += 1
        elif not token and not argv[i].startswith('-'):
            token = argv[i]
            i += 1
        else:
            i += 1

    if not token:
        token = get_hf_token()
    if not token:
        print("No HF token found.", file=sys.stderr)
        print("Usage: hcom relay hf <token>", file=sys.stderr)
        print("   or: huggingface-cli login", file=sys.stderr)
        return 1

    # Get username
    print("Getting HF username...")
    try:
        req = urllib.request.Request(
            "https://huggingface.co/api/whoami-v2",
            headers={"Authorization": f"Bearer {token}"}
        )
        with urllib.request.urlopen(req, timeout=10) as resp:
            data = json.loads(resp.read())
            username = data.get("name")
    except Exception as e:
        print(f"Failed to get username: {e}", file=sys.stderr)
        return 1

    target_space = f"{username}/{space_name}"
    space_url = f"https://{username}-{space_name}.hf.space/"
    created = False

    # Check if Space already exists
    space_exists = False
    try:
        req = urllib.request.Request(
            f"https://huggingface.co/api/spaces/{target_space}",
            headers={"Authorization": f"Bearer {token}"}
        )
        with urllib.request.urlopen(req, timeout=5) as resp:
            if resp.status == 200:
                space_exists = True
    except urllib.error.HTTPError as e:
        if e.code != 404:
            print(f"Check failed: {e}", file=sys.stderr)
            return 1

    # Handle --update: manual instructions (delete requires admin permission)
    if space_exists and do_update:
        print(f"Update manually:")
        print(f"  1. Edit: https://huggingface.co/spaces/{target_space}/edit/main/app.py")
        print(f"  2. Copy from: https://huggingface.co/spaces/{SOURCE_SPACE}/raw/main/app.py")
        return 0

    # Create Space if needed
    if space_exists:
        print(f"Space exists: {target_space}")
    else:
        print(f"Creating {target_space}...")
        try:
            req = urllib.request.Request(
                f"https://huggingface.co/api/spaces/{SOURCE_SPACE}/duplicate",
                data=json.dumps({"repository": target_space, "private": True}).encode(),
                method="POST",
                headers={
                    "Authorization": f"Bearer {token}",
                    "Content-Type": "application/json"
                }
            )
            with urllib.request.urlopen(req, timeout=10) as resp:
                if resp.status not in (200, 201):
                    print(f"Create failed: HTTP {resp.status}", file=sys.stderr)
                    return 1
                created = True
        except urllib.error.HTTPError as e2:
            print(f"Create failed: {e2}", file=sys.stderr)
            return 1

    # Update config (save token for private Space auth)
    config = load_config_snapshot()
    config.core.relay = space_url
    config.core.relay_token = token
    save_config_snapshot(config)

    # Clear version cache after update
    if created:
        from ..core.db import kv_set
        kv_set('relay_version_check', '0')
        kv_set('relay_version_outdated', '0')

    print(f"\n{space_url}")
    if created:
        print(f"\nSpace is building (~15 seconds). Check progress:")
        print(f"  https://huggingface.co/spaces/{target_space}")
        print(f"\nConfig updated. Relay will work once Space is running.")
        print(f"\nCheck status: hcom relay")
        print(f"See active instances from other devices: hcom list")
    else:
        print(f"\nConfig updated.")
    return 0


def cmd_config(argv: list[str]) -> int:
    """Config management: hcom config [key] [value] [--json] [--edit] [--reset]

    Usage:
        hcom config              Show all config (pretty)
        hcom config --json       Show all config (JSON)
        hcom config <key>        Get single value
        hcom config <key> <val>  Set single value
        hcom config --edit       Open in $EDITOR
        hcom config --reset      Reset config to defaults
    """
    import os
    import subprocess
    from ..core.config import (
        load_config_snapshot, save_config_snapshot,
        hcom_config_to_dict, dict_to_hcom_config,
        HcomConfigError, KNOWN_CONFIG_KEYS, DEFAULT_KNOWN_VALUES,
    )
    from ..core.paths import hcom_path, CONFIG_FILE

    # Parse flags
    json_output = '--json' in argv
    edit_mode = '--edit' in argv
    reset_mode = '--reset' in argv
    argv = [a for a in argv if a not in ('--json', '--edit', '--reset')]

    config_path = hcom_path(CONFIG_FILE)

    # --reset: archive and reset to defaults
    if reset_mode:
        return reset_config()

    # --edit: open in editor
    if edit_mode:
        editor = os.environ.get('EDITOR') or os.environ.get('VISUAL')
        if not editor:
            # Try common editors
            for ed in ['code', 'vim', 'nano', 'vi']:
                if shutil.which(ed):
                    editor = ed
                    break
        if not editor:
            print("No editor found. Set $EDITOR or install code/vim/nano", file=sys.stderr)
            return 1

        # Ensure config exists
        if not config_path.exists():
            load_config_snapshot()  # Creates default

        return subprocess.call([editor, str(config_path)])

    # Load current config
    snapshot = load_config_snapshot()
    core_dict = hcom_config_to_dict(snapshot.core)

    # No args: show all
    if not argv:
        if json_output:
            # JSON output: core + extras
            output = {**core_dict, **snapshot.extras}
            print(json.dumps(output, indent=2))
        else:
            # Pretty output
            print(f"Config: {config_path}\n")

            # Core hcom settings
            print("hcom Settings:")
            for key in KNOWN_CONFIG_KEYS:
                value = core_dict.get(key, '')
                default = DEFAULT_KNOWN_VALUES.get(key, '')
                is_default = (value == default) or (not value and not default)
                marker = "" if is_default else " *"
                # Truncate long values
                display_val = value if len(value) <= 60 else value[:57] + "..."
                print(f"  {key}={display_val}{marker}")

            # Extra env vars
            if snapshot.extras:
                print("\nExtra Environment Variables:")
                for key in sorted(snapshot.extras.keys()):
                    value = snapshot.extras[key]
                    display_val = value if len(value) <= 60 else value[:57] + "..."
                    print(f"  {key}={display_val}")

            print("\n* = modified from default")
            print(f"\nEdit: hcom config --edit")
        return 0

    # Single arg: get value
    if len(argv) == 1:
        key = argv[0].upper()
        if not key.startswith('HCOM_'):
            key = f'HCOM_{key}'

        if key in core_dict:
            value = core_dict[key]
        elif key in snapshot.extras:
            value = snapshot.extras[key]
        else:
            # Check if it's a known key with empty value
            if key in KNOWN_CONFIG_KEYS:
                value = ''
            else:
                from .utils import get_command_help
                print(f"Unknown config key: {key}", file=sys.stderr)
                print(f"Valid keys: {', '.join(KNOWN_CONFIG_KEYS)}\n", file=sys.stderr)
                print(get_command_help('config'), file=sys.stderr)
                return 1

        if json_output:
            print(json.dumps({key: value}))
        else:
            print(value)
        return 0

    # Two args: set value
    if len(argv) >= 2:
        key = argv[0].upper()
        if not key.startswith('HCOM_'):
            key = f'HCOM_{key}'

        value = ' '.join(argv[1:])  # Allow spaces in value

        # Validate key - must be a known HCOM config key
        if key not in KNOWN_CONFIG_KEYS:
            from .utils import get_command_help
            print(f"Unknown config key: {key}", file=sys.stderr)
            print(f"Valid keys: {', '.join(sorted(KNOWN_CONFIG_KEYS))}\n", file=sys.stderr)
            print(get_command_help('config'), file=sys.stderr)
            return 1

        # Update config
        new_core_dict = {**core_dict, key: value}
        try:
            new_core = dict_to_hcom_config(new_core_dict)
            snapshot.core = new_core
        except HcomConfigError as e:
            print(f"Invalid value: {e}", file=sys.stderr)
            return 1

        # Save
        save_config_snapshot(snapshot)
        print(f"Set {key}={value}")
        return 0

    return 0
