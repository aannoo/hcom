#!/usr/bin/env python3
"""Headless hook-level smoke test for hcom.

This script exercises the real hook entrypoints (`sessionstart`, `pre`, `poll`,
etc.) without launching Claude. It provides a fast (~3s) regression check for
the messaging pipeline, status updates, and CLI wiring that sit between unit
tests and the full integration suite.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping


# Walk up to repo root (works whether test/ is at top level or under public/)
REPO_ROOT = Path(__file__).resolve().parent
while REPO_ROOT != REPO_ROOT.parent and not (REPO_ROOT / "pyproject.toml").exists():
    REPO_ROOT = REPO_ROOT.parent


@dataclass
class CommandResult:
    """Captured subprocess output."""

    code: int
    stdout: str
    stderr: str


def _ensure_pythonpath(env: Mapping[str, str]) -> dict[str, str]:
    updated = dict(env)
    existing = updated.get("PYTHONPATH")
    repo = str(REPO_ROOT)
    updated["PYTHONPATH"] = repo if not existing else f"{repo}{os.pathsep}{existing}"
    # Inherit HCOM_NATIVE_BIN from parent (set by hdev) for daemon mode
    if "HCOM_NATIVE_BIN" in os.environ:
        updated["HCOM_NATIVE_BIN"] = os.environ["HCOM_NATIVE_BIN"]
    return updated


def _coerce_stdin(stdin: Any | None) -> str | None:
    if stdin is None:
        return None
    if isinstance(stdin, (str, bytes)):
        return stdin if isinstance(stdin, str) else stdin.decode("utf-8")
    if isinstance(stdin, Mapping):
        return json.dumps(stdin)
    raise TypeError(f"Unsupported stdin payload type: {type(stdin)!r}")


def run_hcom(*argv: str, env: Mapping[str, str], stdin: Any | None = None) -> CommandResult:
    """Invoke `python -m src.hcom` with captured output."""

    cmd = [sys.executable, "-m", "src.hcom", *argv]
    prepared_env = _ensure_pythonpath(env)
    text = _coerce_stdin(stdin)
    proc = subprocess.run(
        cmd,
        input=text,
        text=True,
        capture_output=True,
        env=prepared_env,
        cwd=str(REPO_ROOT),
    )
    return CommandResult(code=proc.returncode, stdout=proc.stdout, stderr=proc.stderr)


def write_config(hcom_dir: Path) -> None:
    import tomli_w
    data = {
        "terminal": {"active": "print"},
        "launch": {
            "tag": "", "hints": "Smoke hints", "notes": "",
            "subagent_timeout": 5, "auto_subscribe": "collision",
            "claude": {"args": "Smoke test prompt"},
            "gemini": {"args": "", "system_prompt": ""},
            "codex": {"args": "", "sandbox_mode": "workspace", "system_prompt": ""},
        },
        "relay": {"url": "", "id": "", "token": "", "enabled": True},
        "preferences": {"timeout": 5, "auto_approve": True, "name_export": ""},
    }
    (hcom_dir / "config.toml").write_text(tomli_w.dumps(data), encoding="utf-8")


def _parse_single_json(text: str) -> dict[str, Any]:
    lines = [line for line in text.splitlines() if line.strip()]
    if not lines:
        raise AssertionError("Expected JSON output but stderr/stdout was empty")
    return json.loads(lines[-1])


def bootstrap_instance(
    env: dict[str, str],
    session_id: str,
    transcript: Path,
    workdir: Path,
) -> str:
    """Pre-register instance (like launcher), run sessionstart to bind, return alias."""
    import time

    process_id = env.get("HCOM_PROCESS_ID")
    if not process_id:
        raise AssertionError("HCOM_PROCESS_ID must be set in env")

    # Pre-register instance and process binding (simulates what launcher does)
    # Use hcom's DB functions to ensure schema is initialized
    instance_name = f"smoke-{session_id[:8]}"

    # Run hcom list to init DB, then directly insert the pre-registration
    run_hcom("list", env=env)  # Triggers DB init

    import sqlite3
    db_file = Path(env["HCOM_DIR"]) / "hcom.db"
    conn = sqlite3.connect(str(db_file))
    conn.execute("""
        INSERT OR REPLACE INTO instances
        (name, session_id, status, created_at, last_event_id, directory)
        VALUES (?, NULL, 'starting', ?, 0, ?)
    """, (instance_name, time.time(), str(workdir)))
    conn.execute("""
        INSERT OR REPLACE INTO process_bindings
        (process_id, session_id, instance_name, updated_at)
        VALUES (?, NULL, ?, ?)
    """, (process_id, instance_name, time.time()))
    conn.commit()
    conn.close()

    session_payload = {
        "hook_event_name": "SessionStart",
        "session_id": session_id,
        "transcript_path": str(transcript),
        "cwd": str(workdir),
    }
    result = run_hcom("sessionstart", env=env, stdin=session_payload)
    if result.code != 0:
        raise AssertionError(f"sessionstart failed: {result.stderr}\n{result.stdout}")
    # sessionstart binds session silently (no JSON output) - verify binding was created
    import sqlite3
    db_file = Path(env["HCOM_DIR"]) / "hcom.db"
    conn = sqlite3.connect(str(db_file))
    binding = conn.execute(
        "SELECT instance_name FROM session_bindings WHERE session_id = ?",
        (session_id,)
    ).fetchone()
    conn.close()
    assert binding is not None, f"sessionstart should create session binding for {session_id}"

    prompt_payload = {
        "hook_event_name": "UserPromptSubmit",
        "session_id": session_id,
        "transcript_path": str(transcript),
        "cwd": str(workdir),
        "prompt": "Bootstrapping smoke instance",
    }
    result = run_hcom("userpromptsubmit", env=env, stdin=prompt_payload)
    if result.code != 0:
        raise AssertionError(f"userpromptsubmit failed: {result.stderr}\n{result.stdout}")
    # UserPromptSubmit doesn't output JSON when bootstrap was already handled by SessionStart.
    # It just sets status to 'active' with context 'prompt'. Verify via DB.
    conn = sqlite3.connect(str(db_file))
    status_row = conn.execute(
        "SELECT status, status_context FROM instances WHERE name = ?",
        (instance_name,)
    ).fetchone()
    conn.close()
    assert status_row is not None, "Instance should exist after userpromptsubmit"
    assert status_row[0] == "active", f"Status should be 'active', got '{status_row[0]}'"
    assert status_row[1] == "prompt", f"Status context should be 'prompt', got '{status_row[1]}'"

    return instance_name


def assert_pre_hook_injects(env: dict[str, str], session_id: str) -> None:
    payload = {
        "hook_event_name": "PreToolUse",
        "session_id": session_id,
        "tool_name": "Bash",
        "tool_input": {"command": "hcom send 'ping'"},
    }
    result = run_hcom("pre", env=env, stdin=payload)
    if result.code != 0:
        raise AssertionError(f"pre hook failed: {result.stderr}\n{result.stdout}")
    # PreToolUse may return empty (no injection needed) or JSON with hookSpecificOutput
    # Empty output is valid - just verify hook ran successfully (exit code 0)
    if result.stdout.strip():
        hook_json = _parse_single_json(result.stdout)
        assert "hookSpecificOutput" in hook_json


def send_and_poll(
    env: dict[str, str],
    session_id: str,
    alias: str,
    transcript: Path,
    message: str,
) -> dict[str, Any]:
    send_text = f"@{alias} {message}"
    send_result = run_hcom("send", "-b", send_text, env=env)  # -b for external/human caller
    if send_result.code != 0:
        raise AssertionError(f"send failed: {send_result.stderr}\n{send_result.stdout}")

    poll_payload = {
        "hook_event_name": "Stop",
        "session_id": session_id,
        "transcript_path": str(transcript),
    }
    poll_result = run_hcom("poll", env=env, stdin=poll_payload)
    if poll_result.code != 2:
        raise AssertionError(
            f"poll expected exit code 2, got {poll_result.code}\n{poll_result.stderr}\n{poll_result.stdout}"
        )
    # Hook output is in stdout, not stderr
    delivery = _parse_single_json(poll_result.stdout)
    assert delivery.get("decision") == "block", "Stop hook did not request injection"
    reason_text = delivery.get("reason", "")
    assert message in reason_text, "Delivered message missing from reason"
    assert alias in reason_text, "Alias missing from delivered reason"
    assert "Smoke hints" in delivery.get("reason", ""), "Hints missing from delivery reason"

    # Verify message in SQLite database
    db_path = Path(env["HCOM_DIR"]) / "hcom.db"
    assert db_path.exists(), "hcom.db should exist after message"

    # Query database for the message
    import sqlite3
    conn = sqlite3.connect(str(db_path))
    cursor = conn.execute("SELECT data FROM events WHERE type='message' ORDER BY id DESC LIMIT 10")
    messages = cursor.fetchall()
    conn.close()

    # Check if message is in recent events
    import json
    message_found = False
    for (data_json,) in messages:
        data = json.loads(data_json)
        if send_text in data.get('text', ''):
            message_found = True
            break
    assert message_found, f"Message '{send_text}' not found in hcom.db"

    return delivery


def assert_status(env: dict[str, str], alias: str, expected: str) -> None:
    result = run_hcom("list", "--json", env=env)  # -b for external caller
    if result.code != 0:
        raise AssertionError(f"list --json failed: {result.stderr}\n{result.stdout}")
    instances = _parse_list_json(result.stdout)
    inst = _find_instance(instances, alias)
    if not inst:
        raise AssertionError(f"Alias {alias} missing from status output: {instances}")
    actual = inst.get("status")
    if actual != expected:
        raise AssertionError(f"Expected status {expected!r}, got {actual!r}")


def assert_deleted(env: dict[str, str], alias: str) -> None:
    """Assert instance row is deleted (no longer participating)."""
    result = run_hcom("list", "--json", env=env)  # -b for external caller
    if result.code != 0:
        raise AssertionError(f"list --json failed: {result.stderr}\n{result.stdout}")
    instances = _parse_list_json(result.stdout)
    inst = _find_instance(instances, alias)
    if inst:
        raise AssertionError(f"Instance {alias} should be deleted but found: {inst}")


def _parse_list_json(stdout: str) -> list[dict[str, Any]]:
    """Parse `hcom list --json` output.

    Current behavior: one JSON object per line:
      - Optional first line: {"_self": {...}}
      - Instance lines: {"name": "...", "status": "...", "base_name": "...", ...}

    Legacy behavior (older): {"<name>": {...}} mapping lines.
    """
    items: list[dict[str, Any]] = []
    for line in stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        obj = json.loads(line)
        # Skip _self wrapper line
        if isinstance(obj, dict) and "_self" in obj and len(obj) == 1:
            continue
        items.append(obj)
    return items


def _find_instance(items: list[dict[str, Any]], alias: str) -> dict[str, Any] | None:
    """Find an instance by display name or base_name across supported list --json shapes."""
    for obj in items:
        if not isinstance(obj, dict):
            continue

        # Current shape: instance payload dict
        name = obj.get("name")
        base = obj.get("base_name")
        if alias == name or alias == base:
            return obj

        # Legacy shape: {"alias": {...}}
        if alias in obj and isinstance(obj.get(alias), dict):
            return obj[alias]

    return None


def verify_subagent_flow(
    env: dict[str, str],
    parent_session: str,
    parent_alias: str,
    transcript: Path,
) -> None:
    """Test Task tool subagent creation, messaging, and cleanup"""
    import sqlite3
    import re

    # 0. Task PreToolUse - enter Task context (sets running_tasks='[]')
    task_pre_payload = {
        "hook_event_name": "PreToolUse",
        "session_id": parent_session,
        "transcript_path": str(transcript),
        "tool_name": "Task",
        "tool_input": {"subagent_type": "reviewer"}
    }
    pre_result = run_hcom("pre", env=env, stdin=task_pre_payload)
    if pre_result.code != 0:
        raise AssertionError(f"Task PreToolUse failed: {pre_result.stderr}\n{pre_result.stdout}")

    # 1. SubagentStart hook - lazy creation pattern
    agent_id = "test-agent-123-00000000-0000-0000-0000-000000000001"  # UUID-length for subagent detection
    agent_type = "reviewer"

    subagent_start_payload = {
        "hook_event_name": "SubagentStart",
        "session_id": parent_session,
        "agent_id": agent_id,
        "agent_type": agent_type,
    }
    result = run_hcom("subagent-start", env=env, stdin=subagent_start_payload)
    if result.code != 0:
        raise AssertionError(f"SubagentStart hook failed: {result.stderr}\n{result.stdout}")

    output = _parse_single_json(result.stdout)
    assert "hookSpecificOutput" in output, "SubagentStart should return hook output"
    assert output["hookSpecificOutput"]["hookEventName"] == "SubagentStart"

    # Verify hint shows agent ID
    hint = output["hookSpecificOutput"]["additionalContext"]
    assert agent_id in hint, f"Hint should contain agent ID: {hint}"
    print(f"  ✓ SubagentStart hint: {hint}")

    # 2. Enable subagent (simulate running hcom start with new flags)
    # Set HCOM_NAME and CLAUDECODE to simulate Claude Code context
    env_with_session = env.copy()
    env_with_session["HCOM_NAME"] = parent_alias
    env_with_session["CLAUDECODE"] = "1"

    start_result = run_hcom("start", "--name", agent_id, env=env_with_session)
    if start_result.code != 0:
        raise AssertionError(f"hcom start failed: {start_result.stderr}\n{start_result.stdout}")

    # Extract subagent alias from hcom start output
    # Format: "hcom [already] started for {alias}" or subagent bootstrap "- Your name: {alias}"
    start_output = start_result.stdout.strip()
    match = re.search(r"hcom (?:already )?started for (\S+)", start_output, re.IGNORECASE)
    if not match:
        # Try subagent bootstrap format: "- Your name: {alias}"
        match = re.search(r"- Your name: (\S+)", start_output)
    if not match:
        raise AssertionError(f"Could not extract subagent alias from: {start_output}")
    subagent_id = match.group(1)
    expected_prefix = f"{parent_alias}_{agent_type}_"
    assert subagent_id.startswith(expected_prefix), f"Subagent ID should start with {expected_prefix}, got {subagent_id}"
    print(f"  ✓ Subagent created: {subagent_id}")

    # Verify subagent in DB with correct agent_id
    db_path = Path(env["HCOM_DIR"]) / "hcom.db"
    conn = sqlite3.connect(str(db_path))
    conn.row_factory = sqlite3.Row
    subagent_row = conn.execute("SELECT * FROM instances WHERE name = ?", (subagent_id,)).fetchone()
    assert subagent_row is not None, f"Subagent {subagent_id} should exist in DB"
    assert subagent_row["parent_name"] == parent_alias, f"Parent name should be {parent_alias}"
    assert subagent_row["agent_id"] == agent_id, f"Agent ID should be {agent_id}"
    assert subagent_row["status"] in ('active', 'listening'), "Subagent should have valid status after hcom start"
    print(f"  ✓ Subagent verified in DB with agent_id: {agent_id}")

    # 3. Send message to subagent via @mention
    # Use env without HCOM_PROCESS_ID for external caller (bigboss via -b)
    external_env = {k: v for k, v in env.items() if k != "HCOM_PROCESS_ID"}
    send_text = f"@{subagent_id} Please review this code"
    send_result = run_hcom("send", "-b", send_text, env=external_env)  # -b for external caller
    if send_result.code != 0:
        raise AssertionError(f"send to subagent failed: {send_result.stderr}\n{send_result.stdout}")

    # 4. SubagentStop hook - should deliver messages using agent_id
    subagent_stop_payload = {
        "hook_event_name": "SubagentStop",
        "session_id": parent_session,
        "transcript_path": str(transcript),
        "agent_id": agent_id,  # Claude Code provides agent_id
        "agent_transcript_path": str(transcript.parent / f"agent-{agent_id}.jsonl"),
    }
    stop_result = run_hcom("subagent-stop", env=env, stdin=subagent_stop_payload)
    if stop_result.code != 2:
        raise AssertionError(f"SubagentStop expected exit 2, got {stop_result.code}\n{stop_result.stderr}\n{stop_result.stdout}")

    # SubagentStop outputs to stdout with exit code 2 - delivers messages directly
    stop_output = _parse_single_json(stop_result.stdout)
    assert stop_output.get("decision") == "block", "SubagentStop should block with messages"
    reason_text = stop_output.get("reason", "")
    assert "Please review this code" in reason_text, "Should deliver the sent message"

    # NOTE: Step 4 ("hcom done" polling) removed in agent_id refactor
    # Messages are now delivered directly in SubagentStop (step 3) instead of via PostToolUse polling

    # 5. Task completion PostToolUse - delivers freeze messages to parent
    task_complete_payload = {
        "hook_event_name": "PostToolUse",
        "session_id": parent_session,
        "transcript_path": str(transcript),
        "tool_name": "Task",
        "tool_input": {"subagent_type": agent_type},
        "tool_response": {"agentId": agent_id},
    }
    complete_result = run_hcom("post", env=env, stdin=task_complete_payload)
    if complete_result.code != 0:
        raise AssertionError(f"Task completion failed: {complete_result.stderr}\n{complete_result.stdout}")

    # 6. Final SubagentStop - no pending messages, marks inactive
    final_stop_payload = {
        "hook_event_name": "SubagentStop",
        "session_id": parent_session,
        "transcript_path": str(transcript),
        "agent_id": agent_id,
        "agent_transcript_path": str(transcript.parent / f"agent-{agent_id}.jsonl"),
    }
    import time
    t0 = time.time()
    final_stop = run_hcom("subagent-stop", env=env, stdin=final_stop_payload)
    elapsed = time.time() - t0
    if final_stop.code != 0:
        raise AssertionError(f"Final SubagentStop expected exit 0, got {final_stop.code}\n{final_stop.stderr}\n{final_stop.stdout}")
    # Guard: subagent_timeout=5 in config, so this should take ~5s not 30s
    # If this fails, env var leakage is overriding the test config
    assert elapsed < 10, f"Final SubagentStop took {elapsed:.1f}s — expected <10s (HCOM_SUBAGENT_TIMEOUT=5). Check env var leakage."

    # Verify subagent deleted (row exists = participating, no row = stopped)
    subagent_row = conn.execute("SELECT status FROM instances WHERE name = ?", (subagent_id,)).fetchone()
    assert subagent_row is None, f"Subagent row should be deleted after stop, but found: {subagent_row}"

    conn.close()


def end_session(env: dict[str, str], session_id: str, transcript: Path) -> None:
    payload = {
        "hook_event_name": "SessionEnd",
        "session_id": session_id,
        "transcript_path": str(transcript),
        "reason": "logout",
    }
    result = run_hcom("sessionend", env=env, stdin=payload)
    if result.code != 0:
        raise AssertionError(f"sessionend failed: {result.stderr}\n{result.stdout}")


def run_headless_smoke() -> None:
    session_id = "smoke-session"
    message = "Smoke test message"

    with tempfile.TemporaryDirectory(prefix="hcom_smoke_") as tmp:
        root = Path(tmp)
        hcom_dir = root / ".hcom"
        home_dir = root / "home"
        hcom_dir.mkdir()
        home_dir.mkdir()

        # Import clean_test_env helper from test/conftest.py
        import sys
        test_dir = Path(__file__).resolve().parents[1]
        if str(test_dir) not in sys.path:
            sys.path.insert(0, str(test_dir))
        from conftest import clean_test_env

        # Start with clean environment (all identity vars removed)
        env = clean_test_env()
        process_id = f"smoke-process-{session_id[:8]}"
        env.update(
            {
                "HCOM_DIR": str(hcom_dir),
                "HCOM_LAUNCHED": "1",
                "HCOM_PROCESS_ID": process_id,
                "HOME": str(home_dir),
                # Override any inherited config env vars — config.toml has these
                # but env vars take precedence, so clear them
                "HCOM_TIMEOUT": "5",
                "HCOM_SUBAGENT_TIMEOUT": "5",
                "HCOM_TERMINAL": "print",
            }
        )

        write_config(hcom_dir)

        transcript = home_dir / ".claude" / "projects" / "smoke" / "transcript.jsonl"
        transcript.parent.mkdir(parents=True, exist_ok=True)
        transcript.write_text("{}\n", encoding="utf-8")

        alias = bootstrap_instance(env, session_id, transcript, REPO_ROOT)
        assert_pre_hook_injects(env, session_id)
        delivery = send_and_poll(env, session_id, alias, transcript, message)
        assert_status(env, alias, expected="active")

        # Instance DB should reflect status updates
        import sqlite3
        db_file = Path(env["HCOM_DIR"]) / "hcom.db"
        conn = sqlite3.connect(str(db_file))
        conn.row_factory = sqlite3.Row
        row = conn.execute("SELECT last_stop FROM instances WHERE name = ?", (alias,)).fetchone()
        conn.close()
        assert row and row['last_stop'] > 0, "last_stop not updated"

        # Verify events shows the injected message
        logs_result = run_hcom("events", "--sql", "type = 'message'", "--last", "10", env=env)  # -b for external caller
        if logs_result.code != 0:
            raise AssertionError(f"events --sql type='message' failed: {logs_result.stderr}\n{logs_result.stdout}")
        assert "Smoke test message" in logs_result.stdout, "events missing message"

        # Exercise notification path by simulating a permission block
        notify_payload = {
            "hook_event_name": "Notification",
            "session_id": session_id,
            "transcript_path": str(transcript),
            "message": "Permission denied",
        }
        notify_result = run_hcom("notify", env=env, stdin=notify_payload)
        if notify_result.code != 0:
            raise AssertionError(f"notify failed: {notify_result.stderr}\n{notify_result.stdout}")
        assert_status(env, alias, expected="blocked")

        # Confirm log still contains the original delivery summary
        assert delivery.get("reason"), "Smoke delivery reason unexpectedly empty"

        # Test subagent flow before closing session
        print("\n  Testing subagent flow...")
        verify_subagent_flow(env, session_id, alias, transcript)

        # Close out session - instance row is deleted (row exists = participating)
        end_session(env, session_id, transcript)
        assert_deleted(env, alias)

        poll_payload = {
            "hook_event_name": "Stop",
            "session_id": session_id,
            "transcript_path": str(transcript),
        }
        final_poll = run_hcom("poll", env=env, stdin=poll_payload)
        if final_poll.code != 0:
            raise AssertionError(
                f"final poll expected exit 0, got {final_poll.code}\n{final_poll.stderr}\n{final_poll.stdout}"
            )


def main() -> int:
    run_headless_smoke()
    print("Smoke test passed", flush=True)
    return 0


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())
