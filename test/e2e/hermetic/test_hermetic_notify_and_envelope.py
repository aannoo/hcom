from __future__ import annotations

import json
from uuid import uuid4

import pytest

from .harness import (
    make_workspace,
    run_hcom,
    db_conn,
    seed_instance,
    seed_session_binding,
    parse_single_json,
)


@pytest.fixture
def ws():
    ws = make_workspace(timeout_s=1, hints="Hermetic hints")
    try:
        yield ws
    finally:
        ws.cleanup()


def _hook_payload(ws, *, hook_event_name: str, session_id: str, extra: dict | None = None) -> dict:
    payload = {
        "hook_event_name": hook_event_name,
        "session_id": session_id,
        "transcript_path": str(ws.transcript),
    }
    if extra:
        payload.update(extra)
    return payload


def _latest_message_event(ws) -> tuple[int, dict]:
    conn = db_conn(ws)
    try:
        row = conn.execute(
            "SELECT id, data FROM events WHERE type='message' ORDER BY id DESC LIMIT 1"
        ).fetchone()
        assert row is not None
        return int(row["id"]), json.loads(row["data"])
    finally:
        conn.close()


def test_notify_sets_blocked_and_post_clears_to_active(ws):
    session_id = f"hermetic-notify-{uuid4()}"
    name = "alpha"

    seed_instance(ws, name=name)
    seed_session_binding(ws, session_id=session_id, instance_name=name)

    notify_msg = "Permission denied"
    res = run_hcom(ws.env(), "notify", stdin=_hook_payload(ws, hook_event_name="Notification", session_id=session_id, extra={"message": notify_msg}))
    assert res.code == 0, res.stderr

    conn = db_conn(ws)
    try:
        row = conn.execute(
            "SELECT status, status_context FROM instances WHERE name = ?",
            (name,),
        ).fetchone()
    finally:
        conn.close()

    assert row is not None
    assert row["status"] == "blocked"
    assert row["status_context"] == notify_msg

    # PostToolUse clears blocked → active with approved:<tool>
    post = run_hcom(
        ws.env(),
        "post",
        stdin=_hook_payload(
            ws,
            hook_event_name="PostToolUse",
            session_id=session_id,
            extra={
                "tool_name": "Bash",
                "tool_input": {"command": "echo noop"},
                "tool_response": {"ok": True},
            },
        ),
    )
    assert post.code == 0, post.stderr

    conn = db_conn(ws)
    try:
        row2 = conn.execute(
            "SELECT status, status_context FROM instances WHERE name = ?",
            (name,),
        ).fetchone()
    finally:
        conn.close()

    assert row2 is not None
    assert row2["status"] == "active"
    assert row2["status_context"] == "approved:Bash"


def _set_running_tasks(ws, name: str, running_tasks: dict) -> None:
    conn = db_conn(ws)
    try:
        conn.execute(
            "UPDATE instances SET running_tasks = ? WHERE name = ?",
            (json.dumps(running_tasks), name),
        )
        conn.commit()
    finally:
        conn.close()


def _get_running_tasks(ws, name: str) -> dict:
    conn = db_conn(ws)
    try:
        row = conn.execute("SELECT running_tasks FROM instances WHERE name = ?", (name,)).fetchone()
        return json.loads(row["running_tasks"]) if row and row["running_tasks"] else {}
    finally:
        conn.close()


def test_subagent_stop_cleans_up_no_instance_subagent(ws):
    """Bug fix: subagent_stop must remove agent_id from parent running_tasks
    even when the subagent never created an instance (never ran hcom start)."""
    session_id = f"hermetic-substop-{uuid4()}"
    name = "alpha"
    ghost_agent_id = "ghost-never-started"

    seed_instance(ws, name=name)
    seed_session_binding(ws, session_id=session_id, instance_name=name)

    # Parent has a tracked subagent that never created an instance
    _set_running_tasks(ws, name, {
        "active": True,
        "subagents": [{"agent_id": ghost_agent_id, "type": "explore"}],
    })

    # Verify stuck state
    rt = _get_running_tasks(ws, name)
    assert rt["active"] is True
    assert len(rt["subagents"]) == 1

    # SubagentStop fires for the ghost agent — no instance row exists
    res = run_hcom(
        ws.env(),
        "subagent-stop",
        stdin=_hook_payload(
            ws,
            hook_event_name="SubagentStop",
            session_id=session_id,
            extra={"agent_id": ghost_agent_id},
        ),
    )
    assert res.code == 0, res.stderr

    # Parent's running_tasks should now be cleared
    rt2 = _get_running_tasks(ws, name)
    assert rt2["active"] is False, f"running_tasks.active still True: {rt2}"
    assert len(rt2["subagents"]) == 0


def test_notify_suppressed_in_subagent_context(ws):
    """Bug fix: notify hook must not set blocked on parent when in subagent context.
    Otherwise blocked is set but post (which clears it) is dropped by the subagent path."""
    session_id = f"hermetic-subnotify-{uuid4()}"
    name = "alpha"

    seed_instance(ws, name=name)
    seed_session_binding(ws, session_id=session_id, instance_name=name)

    # Put parent in subagent context
    _set_running_tasks(ws, name, {
        "active": True,
        "subagents": [{"agent_id": "some-task", "type": "explore"}],
    })

    # Notify fires (subagent permission prompt) — should NOT set blocked
    res = run_hcom(
        ws.env(),
        "notify",
        stdin=_hook_payload(
            ws,
            hook_event_name="Notification",
            session_id=session_id,
            extra={"message": "Claude needs your permission to use Bash"},
        ),
    )
    assert res.code == 0, res.stderr

    conn = db_conn(ws)
    try:
        row = conn.execute(
            "SELECT status, status_context FROM instances WHERE name = ?",
            (name,),
        ).fetchone()
    finally:
        conn.close()

    # Status should NOT be blocked — notify was suppressed in subagent context
    assert row is not None
    assert row["status"] != "blocked", f"notify leaked through subagent context: status={row['status']}, context={row['status_context']}"


def test_envelope_ack_requires_reply_to(ws):
    seed_instance(ws, name="alpha")
    res = run_hcom(ws.env(), "send", "--name", "alpha", "--intent", "ack", "@alpha hi")
    assert res.code != 0
    assert "Intent 'ack' requires --reply-to" in (res.stderr + res.stdout)


def test_envelope_thread_inheritance_from_reply_to(ws):
    session_a = f"hermetic-a-{uuid4()}"
    session_b = f"hermetic-b-{uuid4()}"
    a = "alpha"
    b = "bravo"

    seed_instance(ws, name=a)
    seed_instance(ws, name=b)
    seed_session_binding(ws, session_id=session_a, instance_name=a)
    seed_session_binding(ws, session_id=session_b, instance_name=b)

    thread = "t1"

    # Parent message: alpha → bravo with explicit thread
    parent = run_hcom(
        ws.env(),
        "send",
        "--name",
        a,
        "--intent",
        "request",
        "--thread",
        thread,
        f"@{b} parent-msg",
    )
    assert parent.code == 0, parent.stderr
    parent_id, parent_data = _latest_message_event(ws)
    assert parent_data.get("thread") == thread
    assert parent_data.get("intent") == "request"

    # Child message: bravo → alpha ACK, reply-to parent, NO explicit thread => should inherit
    child = run_hcom(
        ws.env(),
        "send",
        "--name",
        b,
        "--intent",
        "ack",
        "--reply-to",
        str(parent_id),
        f"@{a} ack-msg",
    )
    assert child.code == 0, child.stderr
    child_id, child_data = _latest_message_event(ws)
    assert child_id > parent_id
    assert child_data.get("intent") == "ack"
    assert child_data.get("reply_to_local") == parent_id
    assert child_data.get("thread") == thread


