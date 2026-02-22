"""Terminal admin: screen queries, text injection, debug logging."""

from __future__ import annotations

import json
import socket
import time
from pathlib import Path

from ..core.paths import hcom_path


def _flag_path() -> Path:
    return hcom_path(".tmp", "pty_debug_on")


def _get_inject_port(instance_name: str) -> int | None:
    """Look up inject port for instance from notify_endpoints table."""
    try:
        from ..core.db import get_db

        row = get_db().execute(
            "SELECT port FROM notify_endpoints WHERE instance = ? AND kind = 'inject'",
            (instance_name,),
        ).fetchone()
        return row[0] if row else None
    except Exception:
        return None


def _get_pty_instances() -> list[dict]:
    """Get all instances that have inject ports registered."""
    try:
        from ..core.db import get_db

        rows = get_db().execute(
            "SELECT instance, port FROM notify_endpoints WHERE kind = 'inject'"
        ).fetchall()
        return [{"name": r[0], "port": r[1]} for r in rows]
    except Exception:
        return []


def _inject_raw(port: int, data: bytes) -> None:
    """Send data on a single TCP connection and close."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.settimeout(2.0)
        s.connect(("127.0.0.1", port))
        s.sendall(data)


def _inject_text(name: str, text: str, *, enter: bool = False) -> int:
    """Inject text into PTY via inject port.

    Uses separate TCP connections for text and enter (matches delivery system).
    """
    port = _get_inject_port(name)
    if not port:
        print(f"No inject port for '{name}'.")
        return 1
    try:
        if text:
            _inject_raw(port, text.encode("utf-8"))
        if enter:
            if text:
                time.sleep(0.1)
            _inject_raw(port, b"\r")
        label = f"{len(text)} chars" if text else "enter"
        if text and enter:
            label += " + enter"
        print(f"Injected {label} to {name}")
        return 0
    except (socket.error, OSError) as e:
        print(f"Failed to inject to '{name}' (port {port}): {e}")
        return 1


def _query_screen(port: int, timeout: float = 2.0) -> dict | None:
    """Send screen query to inject port, get back parsed JSON."""
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            s.settimeout(timeout)
            s.connect(("127.0.0.1", port))
            s.sendall(b"\x00SCREEN\n")
            s.shutdown(socket.SHUT_WR)
            chunks = []
            while True:
                chunk = s.recv(65536)
                if not chunk:
                    break
                chunks.append(chunk)
        data = b"".join(chunks)
        return json.loads(data) if data else None
    except (socket.error, OSError, ValueError):
        return None


def cmd_term(argv: list[str], **_kw) -> int:
    """Terminal admin: screen query, text injection, debug logging."""
    sub = argv[0] if argv else None

    if sub in ("--help", "-h"):
        from .utils import get_command_help
        print(get_command_help("term"))
        return 0

    if sub == "inject":
        from ..core.instances import resolve_display_name

        enter = "--enter" in argv
        args = [a for a in argv[1:] if a != "--enter"]
        if not args:
            print("Usage: hcom term inject <name> [text] [--enter]")
            return 1
        name = resolve_display_name(args[0]) or args[0]
        text = " ".join(args[1:]) if len(args) > 1 else ""
        if not text and not enter:
            print("Nothing to inject (provide text or --enter)")
            return 1
        return _inject_text(name, text, enter=enter)

    if sub == "debug":
        return _handle_debug(argv[1:])

    # Screen query
    return _handle_screen(argv)


def _handle_debug(argv: list[str]) -> int:
    """Handle: hcom term debug on|off|logs"""
    sub = argv[0] if argv else None

    if sub == "on":
        path = _flag_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        path.touch()
        print("PTY debug logging enabled. Running instances pick up within ~10s.")
        return 0

    if sub == "off":
        path = _flag_path()
        try:
            path.unlink()
        except FileNotFoundError:
            pass
        print("PTY debug logging disabled.")
        return 0

    if sub == "logs":
        return _list_logs()

    status = "on" if _flag_path().exists() else "off"
    print(f"PTY debug logging is {status}. Usage: hcom term debug on|off|logs")
    return 0


def _format_screen(data: dict) -> str:
    """Format screen JSON as readable text."""
    lines = data.get("lines", [])
    cursor = data.get("cursor", [0, 0])
    size = data.get("size", [0, 0])
    out = []
    out.append(f"Screen {size[0]}x{size[1]}  cursor ({cursor[0]},{cursor[1]})")
    out.append(f"ready={data.get('ready')}  prompt_empty={data.get('prompt_empty')}  input_text={data.get('input_text')!r}")
    out.append("")
    for i, line in enumerate(lines):
        if line:
            out.append(f"  {i:3}: {line}")
    return "\n".join(out)


def _handle_screen(argv: list[str]) -> int:
    """Handle: hcom term [name] [--json]"""
    from ..core.instances import resolve_display_name

    raw_json = "--json" in argv
    args = [a for a in argv if a != "--json"]
    name = args[0] if args else None
    if name:
        name = resolve_display_name(name) or name

    if name:
        port = _get_inject_port(name)
        if not port:
            print(f"No inject port for '{name}'. Instance not running or not PTY-managed.")
            return 1
        result = _query_screen(port)
        if not result:
            print(f"No response from '{name}' (port {port}).")
            return 1
        if raw_json:
            print(json.dumps(result))
        else:
            print(_format_screen(result))
        return 0

    # No name — query all PTY instances
    instances = _get_pty_instances()
    if not instances:
        print("No PTY instances found.")
        return 1

    found = False
    for inst in instances:
        result = _query_screen(inst["port"])
        if result:
            if found:
                print()
            if raw_json:
                print(json.dumps({"name": inst["name"], **result}))
            else:
                print(f"[{inst['name']}]")
                print(_format_screen(result))
            found = True
        else:
            print(f"[{inst['name']}] not responding (port {inst['port']})")

    return 0 if found else 1


def _list_logs() -> int:
    debug_dir = hcom_path(".tmp", "logs", "pty_debug")
    if not debug_dir.exists():
        print("No PTY debug logs found.")
        return 0
    logs = sorted(debug_dir.glob("*.log"), key=lambda p: p.stat().st_mtime, reverse=True)
    if not logs:
        print("No PTY debug logs found.")
        return 0
    enabled = _flag_path().exists()
    print(f"Debug logging: {'ON' if enabled else 'OFF'}")
    print(f"Log dir: {debug_dir}")
    for log in logs:
        size = log.stat().st_size
        print(f"  {log.name}  ({size:,} bytes)")
    return 0
