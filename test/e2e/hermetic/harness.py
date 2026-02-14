from __future__ import annotations

import json
import os
import shutil
import sqlite3
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping
from uuid import uuid4


# Walk up to repo root (works whether test/ is at top level or under public/)
REPO_ROOT = Path(__file__).resolve().parent
while REPO_ROOT != REPO_ROOT.parent and not (REPO_ROOT / "pyproject.toml").exists():
    REPO_ROOT = REPO_ROOT.parent
TEST_ROOT = REPO_ROOT / "test"  # May be test/ or test/public/ depending on context


# Keep in sync with `test/conftest.py` identity-clearing behavior.
IDENTITY_ENV_VARS = [
    "CLAUDECODE",
    "HCOM_NAME",
    "HCOM_LAUNCHED",
    "HCOM_LAUNCH_EVENT_ID",
    "HCOM_PTY_MODE",
    "GEMINI_CLI",
    "HCOM_PROCESS_ID",
]


@dataclass(frozen=True)
class CommandResult:
    code: int
    stdout: str
    stderr: str


@dataclass
class HermeticWorkspace:
    root: Path
    home: Path
    hcom_dir: Path
    transcript: Path
    timeout_s: int
    hints: str

    def env(self) -> dict[str, str]:
        env = dict(os.environ)
        for k in IDENTITY_ENV_VARS:
            env.pop(k, None)
        # Isolate from production relay — tests must never push to real server
        env.pop("HCOM_RELAY", None)
        env.pop("HCOM_RELAY_TOKEN", None)
        env.pop("HCOM_RELAY_ENABLED", None)
        env["HOME"] = str(self.home)
        env["HCOM_DIR"] = str(self.hcom_dir)

        # Ensure `python -m src.hcom` can import `src.*`.
        existing = env.get("PYTHONPATH")
        repo = str(REPO_ROOT)
        env["PYTHONPATH"] = repo if not existing else f"{repo}{os.pathsep}{existing}"
        return env

    def cleanup(self) -> None:
        shutil.rmtree(self.root, ignore_errors=True)

    @property
    def db_path(self) -> Path:
        return self.hcom_dir / "hcom.db"


def _init_workspace_db(db_path: Path) -> None:
    """Create hcom DB schema directly (no subprocess)."""
    from hcom.core.db import init_db

    conn = sqlite3.connect(str(db_path))
    try:
        init_db(conn)
        conn.commit()
    finally:
        conn.close()


def make_workspace(*, timeout_s: int = 1, hints: str = "Hermetic hints") -> HermeticWorkspace:
    """Create a fully isolated workspace under `test/output/` (inside the repo)."""
    base = TEST_ROOT / "output" / "hermetic_e2e"
    base.mkdir(parents=True, exist_ok=True)

    root = base / f"ws_{int(time.time())}_{uuid4().hex[:8]}"
    home = root / "home"
    hcom_dir = root / ".hcom"
    transcript = home / ".claude" / "projects" / "hermetic" / "transcript.jsonl"

    home.mkdir(parents=True, exist_ok=True)
    hcom_dir.mkdir(parents=True, exist_ok=True)
    transcript.parent.mkdir(parents=True, exist_ok=True)
    transcript.write_text("{}", encoding="utf-8")  # minimal non-empty file

    _write_config(hcom_dir=hcom_dir, timeout_s=timeout_s, hints=hints)
    ws = HermeticWorkspace(
        root=root,
        home=home,
        hcom_dir=hcom_dir,
        transcript=transcript,
        timeout_s=int(timeout_s),
        hints=hints,
    )

    # Init DB schema directly (avoids ~270ms subprocess per workspace).
    _init_workspace_db(ws.db_path)
    return ws


def _write_config(*, hcom_dir: Path, timeout_s: int, hints: str) -> None:
    import tomli_w
    data = {
        "terminal": {"active": "print"},
        "launch": {
            "tag": "", "hints": hints, "notes": "",
            "subagent_timeout": 1, "auto_subscribe": "collision",
            "claude": {"args": "Hermetic test prompt"},
            "gemini": {"args": "", "system_prompt": ""},
            "codex": {"args": "", "sandbox_mode": "workspace", "system_prompt": ""},
        },
        "relay": {"url": "", "id": "", "token": "", "enabled": True},
        "preferences": {"timeout": int(timeout_s), "auto_approve": True, "name_export": ""},
    }
    (hcom_dir / "config.toml").write_text(tomli_w.dumps(data), encoding="utf-8")


def _coerce_stdin(stdin: Any | None) -> str | None:
    if stdin is None:
        return None
    if isinstance(stdin, (str, bytes)):
        return stdin if isinstance(stdin, str) else stdin.decode("utf-8")
    if isinstance(stdin, Mapping):
        return json.dumps(stdin)
    raise TypeError(f"Unsupported stdin payload type: {type(stdin)!r}")


def run_hcom(env: Mapping[str, str], *argv: str, stdin: Any | None = None) -> CommandResult:
    """Invoke `python -m src.hcom` with captured output (hermetic test runner)."""
    cmd = [sys.executable, "-m", "src.hcom", *argv]
    proc = subprocess.run(
        cmd,
        input=_coerce_stdin(stdin),
        text=True,
        capture_output=True,
        env=dict(env),
        cwd=str(REPO_ROOT),
    )
    return CommandResult(code=proc.returncode, stdout=proc.stdout, stderr=proc.stderr)


def parse_json_lines(text: str) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        out.append(json.loads(line))
    return out


def parse_single_json(text: str) -> dict[str, Any]:
    lines = [l for l in text.splitlines() if l.strip()]
    if not lines:
        raise AssertionError("Expected JSON output but stdout/stderr was empty")
    return json.loads(lines[-1])


def db_conn(ws: HermeticWorkspace) -> sqlite3.Connection:
    assert ws.db_path.exists(), f"Expected DB at {ws.db_path}"
    conn = sqlite3.connect(str(ws.db_path))
    conn.row_factory = sqlite3.Row
    return conn


def seed_instance(
    ws: HermeticWorkspace,
    *,
    name: str,
    tool: str = "claude",
    tag: str | None = None,
    session_id: str | None = None,
    wait_timeout: int | None = None,
) -> None:
    """Create an instance row (row exists = participating)."""
    conn = db_conn(ws)
    try:
        created_at = time.time()
        effective_wait_timeout = int(ws.timeout_s if wait_timeout is None else wait_timeout)
        conn.execute(
            """
            INSERT INTO instances (name, created_at, tool, tag, session_id, transcript_path, directory, wait_timeout)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(name) DO UPDATE SET
                created_at = excluded.created_at,
                tool = excluded.tool,
                tag = excluded.tag,
                session_id = excluded.session_id,
                transcript_path = excluded.transcript_path,
                directory = excluded.directory,
                wait_timeout = excluded.wait_timeout
            """,
            (
                name,
                created_at,
                tool,
                tag,
                session_id,
                str(ws.transcript),
                str(REPO_ROOT),
                effective_wait_timeout,
            ),
        )
        conn.commit()
    finally:
        conn.close()


def seed_session_binding(ws: HermeticWorkspace, *, session_id: str, instance_name: str) -> None:
    """Create session binding (binding existence = hook participation)."""
    conn = db_conn(ws)
    try:
        conn.execute(
            """
            INSERT INTO session_bindings (session_id, instance_name, created_at)
            VALUES (?, ?, ?)
            ON CONFLICT(session_id) DO UPDATE SET
                instance_name = excluded.instance_name,
                created_at = excluded.created_at
            """,
            (session_id, instance_name, time.time()),
        )
        conn.commit()
    finally:
        conn.close()


def seed_process_binding(
    ws: HermeticWorkspace,
    *,
    process_id: str,
    instance_name: str,
    session_id: str | None = None,
) -> None:
    """Create process binding (used by hcom-launched Gemini/Codex hook resolution)."""
    conn = db_conn(ws)
    try:
        conn.execute(
            """
            INSERT INTO process_bindings (process_id, session_id, instance_name, updated_at)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(process_id) DO UPDATE SET
                session_id = excluded.session_id,
                instance_name = excluded.instance_name,
                updated_at = excluded.updated_at
            """,
            (process_id, session_id, instance_name, time.time()),
        )
        conn.commit()
    finally:
        conn.close()


def get_process_binding(ws: HermeticWorkspace, *, process_id: str) -> dict[str, Any] | None:
    conn = db_conn(ws)
    try:
        row = conn.execute(
            "SELECT * FROM process_bindings WHERE process_id = ?",
            (process_id,),
        ).fetchone()
        return dict(row) if row else None
    finally:
        conn.close()


def clear_session_binding(ws: HermeticWorkspace, *, session_id: str) -> None:
    conn = db_conn(ws)
    try:
        conn.execute("DELETE FROM session_bindings WHERE session_id = ?", (session_id,))
        conn.commit()
    finally:
        conn.close()


def get_session_binding(ws: HermeticWorkspace, *, session_id: str) -> str | None:
    conn = db_conn(ws)
    try:
        row = conn.execute(
            "SELECT instance_name FROM session_bindings WHERE session_id = ?",
            (session_id,),
        ).fetchone()
        return str(row["instance_name"]) if row else None
    finally:
        conn.close()


