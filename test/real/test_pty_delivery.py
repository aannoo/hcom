#!/usr/bin/env python3
"""PTY delivery integration test.

Launches a real AI tool instance in tmux, tests message delivery and gate blocking.
Records full screen state at each phase for regression detection.

Requires:
- tmux installed and available
- Target tool CLI installed (claude/gemini/codex)

Phases:
1. Launch tool via `hcom 1 <tool>` with HCOM_TERMINAL=tmux
2. Wait for ready event, capture and validate full screen state
3. Send message → verify delivery via events, capture post-delivery screen
4. Inject uncommitted text → verify gate blocks delivery, capture screen
5. Cleanup

Usage:
    python test/real/test_pty_delivery.py              # claude (default)
    python test/real/test_pty_delivery.py gemini
    python test/real/test_pty_delivery.py codex
    python test/real/test_pty_delivery.py all          # run all three sequentially

    Check logs folder after
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import time
from pathlib import Path

# Import ready patterns from the actual code - single source of truth
from hcom.pty.pty_common import GEMINI_READY_PATTERN, CLAUDE_READY_PATTERN, CODEX_READY_PATTERN

READY_PATTERNS = {
    "claude": CLAUDE_READY_PATTERN.decode(),
    "codex": CODEX_READY_PATTERN.decode(),
    "gemini": GEMINI_READY_PATTERN.decode(),
}

# =============================================================================
# UI ELEMENT MARKERS - Must match src/native/src/pty/screen.rs
#
# These validate that tool TUIs haven't changed. When a test fails here:
#   1. Check if the tool's TUI actually changed (read the debug log output from this test, compare to old)
#   2. If yes: UPDATE screen.rs detection functions, THEN update these markers
#   3. If no: debug why detection is failing
#
# DO NOT just update these markers to make tests pass - that hides real bugs!
# =============================================================================

# Prompt markers: characters that screen.rs scans for to find the input line
# - Claude: get_claude_input_text() at ~line 310 scans for "❯"
# - Codex: get_codex_input_text() at ~line 439 scans for "›" (U+203A)
# - Gemini: get_gemini_input_text() at ~line 401 scans for " > " (new format) or "│ >" (old)
PROMPT_MARKERS = {
    "claude": "❯",
    "codex": "›",
    "gemini": " > ",  # New format (2025+): space + > + space
}

# Frame markers: border characters that help identify the input box structure
# - Claude: horizontal box-drawing char "─" for top/bottom borders
# - Codex: no frame (None)
# - Gemini: "▀" (upper half block) top border (new format) or "╭" corner (old)
FRAME_MARKERS = {
    "claude": "─",
    "codex": None,
    "gemini": "▀",  # New format (2025+): upper half block U+2580
}

# Expected gate block context when prompt has text (src/native/src/delivery.rs)
GATE_BLOCK_CONTEXTS = {
    "claude": "tui:prompt-has-text",   # Claude checks prompt_empty gate
    "codex": "tui:prompt-has-text",     # Codex checks prompt_empty gate
    "gemini": "tui:not-ready",         # Gemini: typing hides ready pattern
}

# Mirror Rust ToolConfig::require_ready_prompt — only gemini gates on ready
REQUIRE_READY = {
    "claude": False,
    "codex": False,
    "gemini": True,
}

# Expected JSON fields from hcom term --json
SCREEN_FIELDS = {"lines", "size", "cursor", "ready", "prompt_empty", "input_text"}

# External sender identity for send commands
SENDER = "ptytest"


def hcom(cmd: str, timeout: int = 15) -> subprocess.CompletedProcess:
    """Run an hcom command, return result."""
    return subprocess.run(
        f"hcom {cmd}", shell=True, capture_output=True, text=True, timeout=timeout,
    )


def hcom_check(cmd: str, timeout: int = 15) -> str:
    """Run and assert success, return stdout."""
    r = hcom(cmd, timeout=timeout)
    if r.returncode != 0:
        fail(f"Command failed: hcom {cmd}\nstderr: {r.stderr}\nstdout: {r.stdout}")
    return r.stdout


def send(msg: str, timeout: int = 15) -> str:
    """Send message using external sender identity."""
    return hcom_check(f"send --from {SENDER} --intent inform '{msg}'", timeout=timeout)


def get_screen(name: str) -> dict:
    """Query PTY screen state as JSON."""
    r = hcom(f"term {name} --json")
    if r.returncode != 0:
        return {}
    try:
        return json.loads(r.stdout)
    except json.JSONDecodeError:
        return {}


def get_events(instance: str, last: int = 20, full: bool = False) -> list[dict]:
    """Get recent events for an instance.

    Args:
        instance: Instance name to query events for
        last: Number of recent events to fetch
        full: If True, use --full flag to get complete event data (including position)
    """
    full_flag = " --full" if full else ""
    r = hcom(f"events --agent {instance} --last {last}{full_flag}")
    if r.returncode != 0:
        return []
    events = []
    for line in r.stdout.strip().splitlines():
        try:
            events.append(json.loads(line.strip()))
        except json.JSONDecodeError:
            continue
    return events


def get_last_event_id(name: str) -> int:
    """Get the last event ID for an instance."""
    events = get_events(name, last=1)
    return events[-1].get("id", 0) if events else 0


def poll_until(fn, description: str, timeout: float = 30, interval: float = 0.5):
    """Poll fn() until truthy, fail on timeout."""
    start = time.time()
    last_val = None
    while time.time() - start < timeout:
        last_val = fn()
        if last_val:
            return last_val
        time.sleep(interval)
    fail(f"Timeout ({timeout}s) waiting for: {description} (last value: {last_val})")


def fail(msg: str):
    print(f"\n  FAIL: {msg}", file=sys.stderr)
    cleanup()
    sys.exit(1)


def ok(msg: str):
    print(f"  OK: {msg}")


# Global for cleanup
_instance_name: str | None = None   # full tagged name (for @mentions, hcom list)
_base_name: str | None = None       # base name (for term, events, kill)


def cleanup():
    name = _base_name or _instance_name
    if name:
        print(f"\nCleaning up {name}...")
        hcom(f"kill {name}")
        time.sleep(1)


# ── Log file for screen snapshots ───────────────────────────────────

_log_file: Path | None = None
_latest_log_file: Path | None = None


def init_log(tool: str):
    """Create log file for screen snapshots and event details.

    Writes to both a timestamped log (gitignored) and a latest log (committed as reference).
    """
    global _log_file, _latest_log_file
    log_dir = Path(__file__).parent / "logs"
    log_dir.mkdir(exist_ok=True)
    ts = time.strftime("%Y%m%d_%H%M%S")
    _log_file = log_dir / f"pty_delivery_{tool}_{ts}.log"
    _latest_log_file = Path(__file__).parent / f"test_pty_delivery_{tool}.latest.log"
    # Clear latest log for fresh run
    _latest_log_file.write_text("")


def log(text: str):
    """Write to both timestamped and latest log files."""
    for path in (_log_file, _latest_log_file):
        if path:
            with open(path, "a") as f:
                f.write(text + "\n")


def log_screen(screen: dict, label: str):
    """Log full screen state to file for regression comparison."""
    log(f"\n── Screen Snapshot: {label}")
    log(f"size: {screen.get('size')}")
    log(f"cursor: {screen.get('cursor')}")
    log(f"ready: {screen.get('ready')}")
    log(f"prompt_empty: {screen.get('prompt_empty')}")
    log(f"input_text: {screen.get('input_text')!r}")
    for i, line in enumerate(screen.get("lines", [])):
        log(f"{i:3d}: {line}")
    log("")


def validate_screen_schema(screen: dict):
    """Verify screen JSON has all expected fields with correct types."""
    missing = SCREEN_FIELDS - set(screen.keys())
    if missing:
        fail(f"Screen JSON missing fields: {missing}")

    assert isinstance(screen["lines"], list), f"lines should be list, got {type(screen['lines'])}"
    assert isinstance(screen["size"], list) and len(screen["size"]) == 2, \
        f"size should be [r,c], got {screen['size']}"
    assert isinstance(screen["cursor"], list) and len(screen["cursor"]) == 2, \
        f"cursor should be [r,c], got {screen['cursor']}"
    assert isinstance(screen["ready"], bool), f"ready should be bool, got {type(screen['ready'])}"
    assert isinstance(screen["prompt_empty"], bool), \
        f"prompt_empty should be bool, got {type(screen['prompt_empty'])}"
    assert screen["input_text"] is None or isinstance(screen["input_text"], str), \
        f"input_text should be str or null, got {type(screen['input_text'])}"


def validate_ready_pattern(screen: dict, tool: str):
    """Verify ready detection is consistent with raw screen content."""
    pattern = READY_PATTERNS[tool]
    screen_text = "\n".join(screen.get("lines", []))
    pattern_present = pattern in screen_text

    if screen["ready"] and not pattern_present:
        fail(f"ready=true but ready pattern '{pattern}' not found in screen lines")
    if not screen["ready"] and pattern_present:
        print(f"  WARN: ready=false but pattern '{pattern}' found in screen (transient?)")


def validate_prompt_consistency(screen: dict):
    """Verify prompt_empty and input_text are consistent."""
    input_text = screen.get("input_text") or ""
    if screen["prompt_empty"] and input_text:
        fail(f"prompt_empty=true but input_text={input_text!r}")
    if not screen["prompt_empty"] and not input_text:
        print(f"  WARN: prompt_empty=false but input_text is empty")


def validate_tool_ui_elements(screen: dict, tool: str):
    """Verify tool-specific TUI elements are present in the screen.

    These are the visual elements our Rust screen parser depends on.
    If the tool's TUI changes these, input extraction will break.
    """
    screen_text = "\n".join(screen.get("lines", []))

    # Check prompt marker
    marker = PROMPT_MARKERS[tool]
    if marker not in screen_text:
        fail(f"Tool prompt marker '{marker}' not found in screen — "
             f"tool TUI may have changed (breaks input extraction in screen.rs)")
    ok(f"Prompt marker '{marker}' present")

    # Check frame element (if applicable)
    frame = FRAME_MARKERS[tool]
    if frame and frame not in screen_text:
        fail(f"Tool frame marker '{frame}' not found in screen — "
             f"tool TUI may have changed (breaks input extraction in screen.rs)")
    if frame:
        ok(f"Frame marker '{frame}' present")


def validate_delivery_events(instance: str, baseline_id: int, sender: str):
    """Verify delivery event was logged with correct structure."""
    events = get_events(instance, last=30, full=True)  # Need full=True for position field
    delivery = None
    for ev in events:
        if (ev.get("id", 0) > baseline_id
                and ev.get("type") == "status"
                and "deliver:" in ev.get("data", {}).get("context", "")):
            delivery = ev
            break

    if not delivery:
        fail(f"No delivery event found after id {baseline_id}")

    data = delivery["data"]
    log(f"Delivery event: {json.dumps(delivery, indent=2)}")
    print(f"  Delivery event: id={delivery['id']} context={data['context']} "
          f"position={data.get('position')} msg_ts={data.get('msg_ts')}")

    # Verify sender in context
    if sender not in data.get("context", ""):
        fail(f"Delivery context '{data['context']}' doesn't reference sender '{sender}'")
    ok(f"Delivery event references sender '{sender}'")

    # Verify position advanced
    if data.get("position", 0) <= baseline_id:
        fail(f"Delivery position {data.get('position')} not after baseline {baseline_id}")
    ok(f"Delivery position {data.get('position')} > baseline {baseline_id}")

    return delivery


def validate_gate_block(instance: str, tool: str, after_id: int):
    """Verify gate block status was set when delivery was blocked."""
    expected_context = GATE_BLOCK_CONTEXTS[tool]
    events = get_events(instance, last=20)

    gate_event = None
    for ev in events:
        if (ev.get("id", 0) > after_id
                and ev.get("type") == "status"
                and ev.get("data", {}).get("context", "").startswith("tui:")):
            gate_event = ev
            break

    if gate_event:
        ctx = gate_event["data"]["context"]
        detail = gate_event["data"].get("detail", "")
        log(f"Gate block event: {json.dumps(gate_event, indent=2)}")
        print(f"  Gate block event: id={gate_event['id']} context={ctx} detail={detail!r}")
        if ctx == expected_context:
            ok(f"Gate blocked with expected context '{expected_context}'")
        else:
            print(f"  WARN: Expected gate context '{expected_context}', got '{ctx}'")
    else:
        # Gate block events only logged on context change — may not appear if already set
        print(f"  INFO: No gate block event found (may already have been in blocked state)")


# ── Main test flow ──────────────────────────────────────────────────

def run_test(tool: str):
    global _instance_name, _base_name
    _instance_name = None
    _base_name = None

    os.environ["HCOM_TERMINAL"] = "tmux"
    os.environ["HCOM_GO"] = "1"
    os.environ["HCOM_TAG"] = "ptytest"
    init_log(tool)

    print("=" * 60)
    print(f"PTY Delivery Test: {tool}")
    print("=" * 60)

    # Record last event ID before launch to filter out old events
    pre_launch_id = 0
    r_pre = hcom("events --last 1")
    if r_pre.returncode == 0:
        for line in r_pre.stdout.strip().splitlines():
            try:
                pre_launch_id = json.loads(line.strip()).get("id", 0)
            except (json.JSONDecodeError, KeyError):
                pass

    # ── Phase 1: Launch ──────────────────────────────────────────
    print(f"\n[Phase 1] Launching {tool} in tmux...")
    t0 = time.time()
    model_flags = {
        "claude": " --model haiku",
        "codex": " --model gpt-5.1-codex-mini",
        "gemini": "",
    }
    extra = model_flags.get(tool, "")
    r = hcom(f"1 {tool}{extra}", timeout=15)
    if r.returncode != 0:
        fail(f"Launch failed: {r.stderr}")

    # Poll for ready event
    print("  Waiting for ready event...")

    def find_ready_instance():
        r = hcom("events --action ready --last 5")
        if r.returncode != 0:
            return None
        for line in reversed(r.stdout.strip().splitlines()):
            line = line.strip()
            if not line:
                continue
            try:
                ev = json.loads(line)
                if (ev.get("type") == "life"
                        and ev.get("data", {}).get("action") == "ready"
                        and ev.get("id", 0) > pre_launch_id):
                    return ev["instance"]
            except (json.JSONDecodeError, KeyError):
                continue
        return None

    _base_name = poll_until(find_ready_instance, "ready event from launched instance", timeout=60, interval=2.0)
    if not _base_name:
        fail("Could not determine instance name from ready event")
    # Ready events use base_name; tagged instances need tag prefix for @mentions
    tag = os.environ.get("HCOM_TAG", "")
    _instance_name = f"{tag}-{_base_name}" if tag else _base_name

    t_ready = time.time() - t0
    ok(f"Instance launched: {_instance_name} (base: {_base_name}, ready in {t_ready:.1f}s)")

    # Wait for screen ready (term uses base name)
    # Timeout allows for slow TUI render; check instance still alive on failure
    screen: dict = poll_until(
        lambda: get_screen(_base_name) or None,
        "screen query returns data",
        timeout=30,
        interval=1.0,
    )
    if not screen:
        # Check if instance is still running
        r = hcom(f"list --json")
        alive = _base_name in r.stdout if r.returncode == 0 else False
        fail(f"Screen query returned empty for 30s (instance alive={alive})")
    if REQUIRE_READY[tool] and not screen.get("ready"):
        fail(f"Screen not ready: {screen}")

    # ── Validate initial screen ──────────────────────────────────
    print(f"\n[Validate] Initial screen state for {tool}...")
    validate_screen_schema(screen)
    ok("Schema valid")
    if REQUIRE_READY[tool]:
        validate_ready_pattern(screen, tool)
        ok(f"Ready pattern '{READY_PATTERNS[tool]}' consistent")
        assert screen["ready"] is True
    else:
        ok(f"ready={screen.get('ready')} (not required for {tool})")
    validate_prompt_consistency(screen)
    ok(f"prompt_empty={screen['prompt_empty']} input_text={screen.get('input_text')!r}")
    validate_tool_ui_elements(screen, tool)
    assert screen["prompt_empty"] is True
    log_screen(screen, f"{tool} — initial (prompt empty)")

    # ── Phase 2: Delivery succeeds on clean prompt ───────────────
    print(f"\n[Phase 2] Testing delivery on clean prompt...")

    baseline_event = get_last_event_id(_base_name)
    ok(f"Baseline event ID: {baseline_event}")

    t1 = time.time()
    send(f"@{_instance_name} delivery-test-1 do not reply")
    ok("Message sent")

    # Wait for delivery event (not just any event - transcript watcher fires prompt events faster)
    def find_delivery_event():
        events = get_events(_base_name, last=30, full=True)
        for ev in events:
            if (ev.get("id", 0) > baseline_event
                    and ev.get("type") == "status"
                    and "deliver:" in ev.get("data", {}).get("context", "")):
                return ev
        return None

    delivery_event = poll_until(find_delivery_event, "delivery event", timeout=20, interval=1.0) # should happen in like 5-10s max unless some rare delay
    t_delivery = time.time() - t1
    new_event = delivery_event["id"]
    ok(f"Cursor advanced: {baseline_event} -> {new_event} (delivery in {t_delivery:.1f}s)")

    # Then wait for screen to settle (tool finishes processing)
    def _screen_settled_after_delivery():
        s = get_screen(_base_name)
        if not s or not s.get("prompt_empty"):
            return False
        if REQUIRE_READY[tool] and not s.get("ready"):
            return False
        return True

    poll_until(_screen_settled_after_delivery, "screen settles after delivery", timeout=60, interval=1.0)

    # Validate delivery event structure
    validate_delivery_events(_base_name, baseline_event, SENDER)

    # Capture and validate post-delivery screen
    screen = get_screen(_base_name)
    validate_screen_schema(screen)
    if REQUIRE_READY[tool]:
        validate_ready_pattern(screen, tool)
    validate_prompt_consistency(screen)
    validate_tool_ui_elements(screen, tool)
    log_screen(screen, f"{tool} — post-delivery")

    # ── Phase 3: Delivery blocked by uncommitted text ────────────
    print(f"\n[Phase 3] Testing delivery blocked by uncommitted text...")

    def _screen_ready_for_inject():
        s = get_screen(_base_name)
        if not s or not s.get("prompt_empty"):
            return False
        if REQUIRE_READY[tool] and not s.get("ready"):
            return False
        return True

    poll_until(_screen_ready_for_inject, "prompt empty before inject", timeout=30, interval=1.0)
    # Extra settle time — tool may still be processing internally
    time.sleep(2)

    # Inject uncommitted text (no enter)
    hcom_check(f"term inject {_base_name} uncommitted text here")
    ok("Injected uncommitted text")

    # Verify text appears in input box
    screen = poll_until(
        lambda: get_screen(_base_name)
                if (get_screen(_base_name).get("input_text") or "")
                   and "uncommitted" in (get_screen(_base_name).get("input_text") or "")
                else None,
        "injected text visible in input box",
        timeout=10,
    )

    # Validate injected state
    validate_screen_schema(screen)
    assert screen["prompt_empty"] is False, f"Expected prompt_empty=false after inject"
    assert "uncommitted" in (screen["input_text"] or ""), f"input_text={screen['input_text']!r}"
    validate_prompt_consistency(screen)
    validate_ready_pattern(screen, tool)
    ok(f"Input text detected: {screen['input_text']!r}")
    log_screen(screen, f"{tool} — after inject (uncommitted text)")

    # Record baseline
    baseline_event2 = get_last_event_id(_base_name)

    # Send message (should be blocked by gate)
    send(f"@{_instance_name} delivery-test-2-should-block do not reply")
    ok("Message sent (should be blocked)")

    # Wait and verify delivery does NOT happen
    print("  Waiting 8s to confirm no delivery...")
    time.sleep(8)

    # Verify: uncommitted text still there
    screen = get_screen(_base_name)
    validate_screen_schema(screen)
    if screen.get("input_text") and "uncommitted" in screen["input_text"]:
        ok(f"Uncommitted text preserved: {screen['input_text']!r}")
    else:
        fail(f"Uncommitted text was clobbered! input_text={screen.get('input_text')!r}")

    validate_prompt_consistency(screen)

    # Verify: no delivery event occurred during gate block
    # (other events like transcript watcher status updates are expected and fine)
    events_after = get_events(_base_name, last=20)
    delivery_during_block = [
        ev for ev in events_after
        if ev.get("id", 0) > baseline_event2
        and ev.get("type") == "status"
        and "deliver:" in ev.get("data", {}).get("context", "")
    ]
    if delivery_during_block:
        fail(f"Unexpected delivery during gate block: {delivery_during_block[0]}")
    ok("No delivery event during gate block")

    # Verify gate block event
    validate_gate_block(_base_name, tool, baseline_event2)

    log_screen(screen, f"{tool} — gate blocked (text preserved)")

    # ── Phase 4: Submit uncommitted text, unblock delivery ────────
    print(f"\n[Phase 4] Submitting uncommitted text, waiting for blocked message delivery...")

    baseline_event3 = get_last_event_id(_base_name)

    # Submit the existing uncommitted text (clears prompt, unblocks gate)
    hcom_check(f"term inject {_base_name} --enter")
    ok("Sent --enter to submit uncommitted text")

    # Wait for tool to process and return to ready
    def _screen_settled_after_submit():
        s = get_screen(_base_name)
        if not s or not s.get("prompt_empty"):
            return False
        if REQUIRE_READY[tool] and not s.get("ready"):
            return False
        return True

    poll_until(_screen_settled_after_submit, "screen settles after submitting text", timeout=60, interval=1.0)

    # Wait for delivery event for the previously-blocked message
    # (delivery thread stays in Pending with 2s retry — should fire within seconds of gate unblock)
    # (must check for deliver: context specifically — transcript watcher events don't count)
    def find_delivery_phase4():
        evs = get_events(_base_name, last=20)
        for ev in evs:
            if (ev.get("id", 0) > baseline_event3
                    and ev.get("type") == "status"
                    and "deliver:" in ev.get("data", {}).get("context", "")):
                return ev
        return None

    delivery3 = poll_until(find_delivery_phase4, "delivery event for blocked message", timeout=60, interval=1.0)
    ok(f"Blocked message delivered: id={delivery3['id']} context={delivery3['data']['context']}")
    log(f"Phase 4 delivery event: {json.dumps(delivery3, indent=2)}")

    # Capture final screen
    screen = get_screen(_base_name)
    validate_screen_schema(screen)
    if REQUIRE_READY[tool]:
        validate_ready_pattern(screen, tool)
    validate_prompt_consistency(screen)
    log_screen(screen, f"{tool} — after blocked message delivered")

    # Log all events for this instance as reference
    all_events = get_events(_base_name, last=50)
    log(f"\n── All events for {_instance_name}")
    for ev in all_events:
        log(json.dumps(ev))

    # ── Cleanup ──────────────────────────────────────────────────
    print(f"\n[Cleanup] Stopping {_instance_name}...")
    cleanup()
    _instance_name = None
    _base_name = None

    print("\n" + "=" * 60)
    print(f"{tool.upper()} — ALL PHASES PASSED")
    if _log_file:
        print(f"  Log: {_log_file}")
    print("=" * 60)


def main():
    tools = sys.argv[1:] or ["claude"]
    if tools == ["all"]:
        tools = ["claude", "gemini", "codex"]

    for tool in tools:
        if tool not in READY_PATTERNS:
            print(f"Unknown tool: {tool}. Use: claude, gemini, codex, all", file=sys.stderr)
            sys.exit(1)

    results = {}
    for tool in tools:
        try:
            run_test(tool)
            results[tool] = "PASS"
        except SystemExit:
            results[tool] = "FAIL"
            if len(tools) == 1:
                raise
        except Exception as e:
            print(f"\nUnexpected error for {tool}: {e}", file=sys.stderr)
            import traceback
            traceback.print_exc()
            cleanup()
            results[tool] = "FAIL"

    if len(tools) > 1:
        print("\n" + "=" * 60)
        print("SUMMARY")
        for tool, result in results.items():
            print(f"  {tool}: {result}")
        print("=" * 60)
        if any(r == "FAIL" for r in results.values()):
            sys.exit(1)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\nInterrupted")
        cleanup()
        sys.exit(1)
