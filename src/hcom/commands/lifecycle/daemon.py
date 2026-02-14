"""Daemon management commands."""

import os
import sys
import time
from pathlib import Path

from ...shared import CommandContext


def cmd_daemon(argv: list[str], *, ctx: CommandContext | None = None) -> int:
    """Manage the hcom daemon for fast hook/CLI handling.

    Usage:
        hcom daemon status    # Show daemon status
        hcom daemon start     # Start daemon
        hcom daemon stop      # Stop daemon (auto-escalates to SIGKILL after 5s)
        hcom daemon restart   # Restart daemon

    The daemon provides <20ms latency for hooks and CLI commands by pre-loading
    Python modules. It listens on ~/.hcom/hcomd.sock (or $HCOM_DIR/hcomd.sock).
    """
    _ = ctx  # Unused but required by command signature

    if not argv:
        argv = ["status"]

    subcmd = argv[0]

    if subcmd == "status":
        return _daemon_status()
    elif subcmd == "start":
        return _daemon_start()
    elif subcmd == "stop":
        return _daemon_stop()
    elif subcmd == "restart":
        _daemon_stop()
        time.sleep(0.5)
        return _daemon_start()
    else:
        print(f"Unknown daemon subcommand: {subcmd}", file=sys.stderr)
        print("Usage: hcom daemon [status|start|stop|restart]", file=sys.stderr)
        return 1


def _get_daemon_paths() -> tuple[Path, Path]:
    """Get daemon socket and PID file paths."""
    hcom_dir_str = os.environ.get("HCOM_DIR")
    if hcom_dir_str:
        base = Path(hcom_dir_str).expanduser()
    else:
        base = Path.home() / ".hcom"
    return base / "hcomd.sock", base / "hcomd.pid"


def _daemon_status() -> int:
    """Show daemon status."""
    from ...daemon import get_daemon_info

    info = get_daemon_info()

    if not info["running"]:
        if info["pid"] is None:
            print("Daemon: not running")
        else:
            print("Daemon: stale PID file (process not running)")
        return 0

    print(f"Daemon: running (PID {info['pid']})")
    print(f"Socket: {info['socket_path']}")

    if info["responsive"]:
        print("Status: responsive")
    else:
        print("Status: unresponsive (socket exists but not accepting connections)")
    return 0


def _daemon_start() -> int:
    """Start the daemon."""
    import subprocess

    socket_path, pid_path = _get_daemon_paths()

    # Check if already running
    if pid_path.exists():
        try:
            pid = int(pid_path.read_text().strip())
            os.kill(pid, 0)
            print(f"Daemon already running (PID {pid})")
            return 0
        except (ProcessLookupError, ValueError):
            # Stale PID file, clean up
            pid_path.unlink(missing_ok=True)
            socket_path.unlink(missing_ok=True)

    # Start daemon
    try:
        proc = subprocess.Popen(
            [sys.executable, "-m", "hcom.daemon"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        # Wait briefly for daemon to start
        for _ in range(10):
            time.sleep(0.1)
            if pid_path.exists():
                pid = int(pid_path.read_text().strip())
                print(f"Daemon started (PID {pid})")
                return 0

        # Check if process is still alive
        if proc.poll() is None:
            print(f"Daemon started (PID {proc.pid})")
            return 0
        else:
            print("Daemon failed to start", file=sys.stderr)
            return 1
    except (ProcessLookupError, ValueError, OSError) as e:
        print(f"Failed to start daemon: {e}", file=sys.stderr)
        return 1


def _daemon_stop() -> int:
    """Stop the daemon.

    Sends SIGTERM and waits up to 5 seconds for graceful shutdown.
    If daemon doesn't respond, automatically escalates to SIGKILL.
    """
    import signal

    _, pid_path = _get_daemon_paths()

    if not pid_path.exists():
        print("Daemon not running")
        return 0

    try:
        pid = int(pid_path.read_text().strip())
        os.kill(pid, signal.SIGTERM)
        print(f"Sent SIGTERM to daemon (PID {pid})")

        # Wait for graceful shutdown (5 seconds total)
        for _ in range(50):
            time.sleep(0.1)
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                print("Daemon stopped")
                return 0

        # Auto-escalate to SIGKILL if daemon didn't respond
        print("Daemon did not respond to SIGTERM, escalating to SIGKILL")
        try:
            os.kill(pid, signal.SIGKILL)
            print("Daemon killed (SIGKILL)")
        except ProcessLookupError:
            print("Daemon stopped")
        return 0
    except ProcessLookupError:
        print("Daemon not running (stale PID file)")
        pid_path.unlink(missing_ok=True)
        return 0
    except ValueError:
        print("Invalid PID file", file=sys.stderr)
        return 1
    except (ProcessLookupError, OSError) as e:
        print(f"Failed to stop daemon: {e}", file=sys.stderr)
        return 1
