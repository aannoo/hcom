"""Reset commands for HCOM"""

import sqlite3
import sys
import time
import shutil
from datetime import datetime
from .utils import format_error, get_help_text
from ..core.paths import hcom_path, LAUNCH_DIR, LOGS_DIR, ARCHIVE_DIR
from ..shared import shorten_path, CommandContext, is_inside_ai_tool
from ..core.thread_context import get_hcom_go


def get_archive_timestamp() -> str:
    """Get timestamp for archive files"""
    return datetime.now().strftime("%Y-%m-%d_%H%M%S")


def cmd_help() -> int:
    """Show help text"""
    print(get_help_text())
    return 0


def clear() -> int:
    """Clear and archive conversation"""
    from ..core.db import DB_FILE, close_db, get_db, archive_db

    db_file = hcom_path(DB_FILE)

    # Cleanup: temp files, old scripts, old background logs
    cutoff_time_24h = time.time() - (24 * 60 * 60)  # 24 hours ago
    cutoff_time_30d = time.time() - (30 * 24 * 60 * 60)  # 30 days ago

    launch_dir = hcom_path(LAUNCH_DIR)
    if launch_dir.exists():
        for f in launch_dir.glob("*"):
            if f.is_file() and f.stat().st_mtime < cutoff_time_24h:
                f.unlink(missing_ok=True)

    # Clean system prompt temp files older than 24h
    prompts_dir = hcom_path(".tmp", "prompts")
    if prompts_dir.exists():
        for f in prompts_dir.glob("*.md"):
            if f.stat().st_mtime < cutoff_time_24h:
                f.unlink(missing_ok=True)

    # Clean background logs older than 30 days
    logs_dir = hcom_path(LOGS_DIR)
    if logs_dir.exists():
        for f in logs_dir.glob("background_*.log"):
            if f.stat().st_mtime < cutoff_time_30d:
                f.unlink(missing_ok=True)

    # Check if DB exists
    if not db_file.exists():
        print("No HCOM conversation to clear")
        return 0

    try:
        # Check if DB has content worth archiving
        db = get_db()
        event_count = db.execute("SELECT COUNT(*) FROM events").fetchone()[0]
        instance_count = db.execute("SELECT COUNT(*) FROM instances").fetchone()[0]

        if event_count > 0 or instance_count > 0:
            # Archive using centralized function
            archive_path = archive_db("reset")
            if archive_path:
                print(f"Archived to {shorten_path(archive_path)}/")
                print("Started fresh HCOM conversation")
            else:
                # archive_db returned None = DB locked (Windows), delete pending
                print("Archived (original DB locked, will clear on next run)")
        else:
            # Empty DB, just delete
            close_db()
            db_file.unlink()
            db_wal = hcom_path(f"{DB_FILE}-wal")
            db_shm = hcom_path(f"{DB_FILE}-shm")
            db_wal.unlink(missing_ok=True)
            db_shm.unlink(missing_ok=True)
            print("Started fresh HCOM conversation")
        return 0

    except (OSError, sqlite3.Error) as e:
        print(format_error(f"Failed to archive: {e}"), file=sys.stderr)
        return 1



def reset_config() -> int:
    """Archive and reset config to defaults. Returns exit code."""
    from ..core.paths import CONFIG_TOML, ENV_FILE

    archived = False
    timestamp = datetime.now().strftime("%Y%m%d_%H%M%S")
    archive_config_dir = hcom_path(ARCHIVE_DIR, "config")

    toml_path = hcom_path(CONFIG_TOML)
    if toml_path.exists():
        archive_config_dir.mkdir(parents=True, exist_ok=True)
        shutil.copy2(toml_path, archive_config_dir / f"config.toml.{timestamp}")
        toml_path.unlink()
        print(f"Config archived to archive/config/config.toml.{timestamp}")
        archived = True

    env_path = hcom_path(ENV_FILE)
    if env_path.exists():
        archive_config_dir.mkdir(parents=True, exist_ok=True)
        shutil.copy2(env_path, archive_config_dir / f"env.{timestamp}")
        env_path.unlink()
        if not archived:
            print(f"Env archived to archive/config/env.{timestamp}")
        archived = True

    if not archived:
        print("No config file to reset")
    return 0


def _print_reset_preview(target: str | None) -> None:
    """Print reset preview for AI tools. Shows what will be destroyed."""
    from ..core.db import DB_FILE, get_db, iter_instances
    from ..core.tool_utils import build_hcom_command

    hcom_cmd = build_hcom_command()
    db_file = hcom_path(DB_FILE)

    # Count instances and events
    instance_count = 0
    event_count = 0
    local_instances: list[str] = []

    if db_file.exists():
        try:
            db = get_db()
            event_count = db.execute("SELECT COUNT(*) FROM events").fetchone()[0]
            for inst in iter_instances():
                if not inst.get("origin_device_id"):  # Local only
                    instance_count += 1
                    local_instances.append(inst["name"])
        except Exception:
            pass

    # Build preview based on target
    if target == "hooks":
        print(f"""
== RESET HOOKS PREVIEW ==
This will remove hcom hooks from tool configs.

Actions:
  • Remove hooks from Claude Code settings (~/.claude/settings.json)
  • Remove hooks from Gemini CLI settings (~/.gemini/settings.json)
  • Remove hooks from Codex config (~/.codex/)

To reinstall: hcom hooks add

Set HCOM_GO=1 and run again to proceed:
  HCOM_GO=1 {hcom_cmd} reset hooks
""")
    elif target == "all":
        print(f"""
== RESET ALL PREVIEW ==
This will stop all instances, archive the database, remove hooks, and reset config.

Current state:
  • {instance_count} local instance{"s" if instance_count != 1 else ""}: \
{", ".join(local_instances[:5])}{" ..." if len(local_instances) > 5 else "" if local_instances else "(none)"}
  • {event_count} events in database

Actions:
  1. Stop all {instance_count} local instances (kills processes, logs snapshots)
  2. Archive database to ~/.hcom/archive/session-<timestamp>/
  3. Delete database (hcom.db)
  4. Remove hooks from Claude/Gemini/Codex configs
  5. Archive and delete config.toml + env
  6. Clear device identity (new UUID on next relay)

Set HCOM_GO=1 and run again to proceed:
  HCOM_GO=1 {hcom_cmd} reset all
""")
    else:
        # Plain reset (archive db)
        print(f"""
== RESET PREVIEW ==
This will archive and clear the current hcom session.

Current state:
  • {instance_count} instance{"s" if instance_count != 1 else ""}: {", ".join(local_instances[:5])}{" ..." if len(local_instances) > 5 else "" if local_instances else "(none)"}
  • {event_count} events in database

Actions:
  1. Archive database to ~/.hcom/archive/session-<timestamp>/
  2. Delete database (hcom.db, hcom.db-wal, hcom.db-shm)
  3. Log reset event to fresh database
  4. Sync with relay (push reset, pull fresh state)

Note: Instance rows are deleted but snapshots preserved in archive.
      Query archived sessions with: {hcom_cmd} archive

Set HCOM_GO=1 and run again to proceed:
  HCOM_GO=1 {hcom_cmd} reset
""")


def cmd_reset(argv: list[str], *, ctx: CommandContext | None = None) -> int:
    """Reset HCOM components.

    Usage:
        hcom reset              Clear database (archive conversation)
        hcom reset hooks        Remove hooks
        hcom reset all          Stop all + clear db + remove hooks + reset config

    Note: Hooks are auto-installed on any hcom command. Use 'reset hooks' to remove.
    """
    from .lifecycle import cmd_stop
    from .utils import get_command_help

    # Handle --help
    if "--help" in argv or "-h" in argv:
        print(get_command_help("reset"))
        return 0

    # Parse subcommand
    target = None

    for arg in argv:
        if target is None and not arg.startswith("-"):
            target = arg
        else:
            print(f"Unknown argument: {arg}\n", file=sys.stderr)
            print(get_command_help("reset"), file=sys.stderr)
            return 1

    # Validate
    if target and target not in ("hooks", "all"):
        print(f"Unknown target: {target}\n", file=sys.stderr)
        print(get_command_help("reset"), file=sys.stderr)
        return 1

    # Confirmation gate: inside AI tools, require HCOM_GO=1
    if is_inside_ai_tool() and not get_hcom_go():
        _print_reset_preview(target)
        return 0

    exit_codes = []

    # hooks: remove hooks from all locations
    if target == "hooks":
        from .hooks_cmd import cmd_hooks_remove

        return cmd_hooks_remove(["all"])

    # Stop all instances before clearing database (prevents zombie processes)
    exit_codes.append(cmd_stop(["all"]))

    # Stop daemon if running
    from .lifecycle import _daemon_stop
    _daemon_stop()

    # Clear database
    exit_codes.append(clear())

    # Log reset event (used for local import filtering + relay to other devices)
    from ..core.db import log_reset_event

    log_reset_event()

    # Push reset event to relay server
    try:
        from ..relay import trigger_push

        trigger_push()
    except Exception:
        pass  # Best effort

    # all: also remove hooks, reset config, clear device identity
    if target == "all":
        # Clear device identity (new UUID on next relay push)
        device_id_file = hcom_path(".tmp", "device_id")
        if device_id_file.exists():
            device_id_file.unlink()

        # Clear instance counter (reset first-time hints)
        from ..core.paths import FLAGS_DIR

        instance_count_file = hcom_path(FLAGS_DIR, "instance_count")
        if instance_count_file.exists():
            instance_count_file.unlink()

        from .hooks_cmd import cmd_hooks_remove

        if cmd_hooks_remove(["all"]) != 0:
            exit_codes.append(1)

        exit_codes.append(reset_config())

    return max(exit_codes) if exit_codes else 0
