from __future__ import annotations

import time
from uuid import uuid4

import pytest

from .harness import (
    make_workspace,
    run_hcom,
    parse_single_json,
    seed_instance,
    seed_session_binding,
    clear_session_binding,
    get_session_binding,
)


@pytest.fixture
def ws():
    ws = make_workspace(timeout_s=1, hints="Hermetic hints")
    try:
        yield ws
    finally:
        ws.cleanup()


def _hook_poll_payload(ws, session_id: str) -> dict:
    return {
        "hook_event_name": "Stop",
        "session_id": session_id,
        "transcript_path": str(ws.transcript),
    }


def test_poll_delivers_exit_code_2_and_json(ws):
    session_id = f"hermetic-{uuid4()}"
    name = "herm1"

    seed_instance(ws, name=name)
    seed_session_binding(ws, session_id=session_id, instance_name=name)

    send = run_hcom(ws.env(), "send", "-b", f"@{name} hello hermetic")
    assert send.code == 0, send.stderr

    poll = run_hcom(ws.env(), "poll", stdin=_hook_poll_payload(ws, session_id))
    assert poll.code == 2, poll.stderr

    out = parse_single_json(poll.stdout)
    assert out["decision"] == "block"
    assert "hello hermetic" in out["reason"]


def test_broadcast_delivers_to_all_participants(ws):
    s1 = f"hermetic-a-{uuid4()}"
    s2 = f"hermetic-b-{uuid4()}"
    a = "alpha"
    b = "bravo"

    seed_instance(ws, name=a)
    seed_instance(ws, name=b)
    seed_session_binding(ws, session_id=s1, instance_name=a)
    seed_session_binding(ws, session_id=s2, instance_name=b)

    send = run_hcom(ws.env(), "send", "-b", "broadcast-hello")
    assert send.code == 0, send.stderr

    poll_a = run_hcom(ws.env(), "poll", stdin=_hook_poll_payload(ws, s1))
    assert poll_a.code == 2
    assert "broadcast-hello" in parse_single_json(poll_a.stdout)["reason"]

    poll_b = run_hcom(ws.env(), "poll", stdin=_hook_poll_payload(ws, s2))
    assert poll_b.code == 2
    assert "broadcast-hello" in parse_single_json(poll_b.stdout)["reason"]


def test_mentions_deliver_only_to_target(ws):
    # Expect non-target poll to time out quickly (timeout is 1s in config.toml).
    s1 = f"hermetic-a-{uuid4()}"
    s2 = f"hermetic-b-{uuid4()}"
    a = "alpha"
    b = "bravo"

    seed_instance(ws, name=a)
    seed_instance(ws, name=b)
    seed_session_binding(ws, session_id=s1, instance_name=a)
    seed_session_binding(ws, session_id=s2, instance_name=b)

    send = run_hcom(ws.env(), "send", "-b", f"@{a} private-hello")
    assert send.code == 0, send.stderr

    poll_a = run_hcom(ws.env(), "poll", stdin=_hook_poll_payload(ws, s1))
    assert poll_a.code == 2
    assert "private-hello" in parse_single_json(poll_a.stdout)["reason"]

    start = time.time()
    poll_b = run_hcom(ws.env(), "poll", stdin=_hook_poll_payload(ws, s2))
    elapsed = time.time() - start
    assert poll_b.code == 0
    # Keep this loose: just ensure it didn't hang for a long time.
    assert elapsed < 3.0
    assert poll_b.stdout.strip() == ""


def test_invalid_mentions_fail_strict(ws):
    seed_instance(ws, name="alpha")

    send = run_hcom(ws.env(), "send", "-b", "@does-not-exist hello")
    assert send.code != 0
    assert "@mentions to non-existent or stopped agents" in (send.stderr + send.stdout)


def test_transcript_marker_binding_fallback_and_precedence(ws):
    # Marker binding only triggers if there are pending instances:
    # instances.session_id IS NULL AND tool != 'adhoc'
    session_id = f"hermetic-marker-{uuid4()}"
    a = "alpha"
    b = "bravo"

    # Seed 2 instances; make `bravo` pending by leaving session_id NULL.
    seed_instance(ws, name=a, session_id=None)
    seed_instance(ws, name=b, session_id=None, tool="claude")

    # 1) With an existing session binding, marker should NOT override.
    seed_session_binding(ws, session_id=session_id, instance_name=a)
    ws.transcript.write_text(f"[HCOM:BIND:{b}]\n", encoding="utf-8")

    pre = run_hcom(
        ws.env(),
        "pre",
        stdin={
            "hook_event_name": "PreToolUse",
            "session_id": session_id,
            "transcript_path": str(ws.transcript),
            "tool_name": "Bash",
            "tool_input": {"command": "echo noop"},
        },
    )
    assert pre.code == 0, pre.stderr
    assert get_session_binding(ws, session_id=session_id) == a

    # 2) Without session binding, marker should bind to the pending instance.
    clear_session_binding(ws, session_id=session_id)
    pre2 = run_hcom(
        ws.env(),
        "pre",
        stdin={
            "hook_event_name": "PreToolUse",
            "session_id": session_id,
            "transcript_path": str(ws.transcript),
            "tool_name": "Bash",
            "tool_input": {"command": "echo noop"},
        },
    )
    assert pre2.code == 0, pre2.stderr
    assert get_session_binding(ws, session_id=session_id) == b


