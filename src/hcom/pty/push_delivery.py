"""Notify-driven delivery engine for PTY-based tool integrations.

This module is intentionally self-contained so Gemini/Codex/Claude-PTY can share:
- a single definition of "safe to inject"
- a notify-driven loop (no periodic DB polling when idle)
- bounded retry behavior when delivery is pending but unsafe

Used by: pty_handler.py (Claude, Gemini, Codex PTY runners)
"""

from __future__ import annotations

from dataclasses import dataclass
import time
from typing import Callable, Protocol

from ..core.timeouts import (
    OUTPUT_STABLE_SECONDS,
    INITIAL_RETRY_DELAY,
    VERIFY_CURSOR_TIMEOUT,
)


class Notifier(Protocol):
    """Wake-up primitive (usually a local TCP notify server)."""

    def wait(self, *, timeout: float) -> bool:
        """Block until notified or timeout. Returns True if notified."""
        ...

    def close(self) -> None: ...


class RetryPolicyProtocol(Protocol):
    """Protocol for retry delay calculation."""

    def delay(self, attempt: int, *, pending_for: float | None = None) -> float:
        """Calculate delay for retry attempt."""
        ...


class PTYLike(Protocol):
    """Subset of PTYWrapper used for safe injection gating."""

    @property
    def actual_port(self) -> int | None: ...

    def is_waiting_approval(self) -> bool: ...

    def is_user_active(self) -> bool: ...

    def is_ready(self) -> bool: ...

    def is_output_stable(self, seconds: float) -> bool: ...

    def is_prompt_empty(self) -> bool: ...

    def get_input_box_text(self) -> str | None: ...


@dataclass(frozen=True, slots=True)
class GateResult:
    safe: bool
    reason: str


@dataclass
class DeliveryLoopState:
    """State tracking for delivery loop debouncing."""

    last_block_reason: str | None = None
    last_block_log: float = 0.0


def _log_gate_block(state: DeliveryLoopState, reason: str, instance_name: str = "") -> None:
    """Log gate block with 5-second debounce for same reason."""
    from ..core.log import log_info

    now = time.monotonic()
    # Only log if reason changed or 5+ seconds since last log
    if reason != state.last_block_reason or (now - state.last_block_log) >= 5.0:
        log_info("pty", "gate.blocked", instance=instance_name, reason=reason)
        state.last_block_reason = reason
        state.last_block_log = now


@dataclass(frozen=True, slots=True)
class DeliveryGate:
    """Conservative 'safe to inject' gate.

    This gate answers one question:
    "If we inject a single line + Enter right now, will it land as a fresh user turn
    without clobbering an approval prompt, a running command, or the user's typing?"

    Gate checks (in order):
    - require_idle: DB status must be "listening" (set by hooks after turn completes).
        Claude/Gemini hooks also set status="blocked" on approval which fails this check.
    - block_on_approval: No pending approval prompt (OSC9 detection in PTY)
    - block_on_user_activity: No keystrokes within cooldown (default 0.5s)
    - require_ready_prompt: Ready pattern visible on screen (e.g., "? for shortcuts").
        Pattern hidden when user has uncommitted text or is in a submenu (slash menu).
        Note: Claude hides this in accept-edits mode, so Claude disables this check.
    - require_prompt_empty: Check if prompt has no user text (Claude-specific).
        Detects static placeholders (Try "...") and LLM suggestions (↵ send).
    - require_output_stable_seconds: Screen unchanged for N seconds (default 1.0s)
    """

    require_idle: bool = False
    require_ready_prompt: bool = True
    require_prompt_empty: bool = False  # Claude enables this
    require_output_stable_seconds: float = OUTPUT_STABLE_SECONDS
    block_on_user_activity: bool = True
    block_on_approval: bool = True

    def evaluate(self, *, wrapper: PTYLike, is_idle: bool | None = None, instance_name: str = "") -> GateResult:
        """Evaluate gate conditions. Returns GateResult with safe=True/False and reason.

        Note: This method does NOT log. Logging is handled by the delivery loop
        via _log_gate_block() with debounce to reduce log spam.
        """
        if self.require_idle and not is_idle:
            return GateResult(False, "not_idle")
        if self.block_on_approval and wrapper.is_waiting_approval():
            return GateResult(False, "approval")
        if self.block_on_user_activity and wrapper.is_user_active():
            return GateResult(False, "user_active")
        if self.require_ready_prompt and not wrapper.is_ready():
            return GateResult(False, "not_ready")
        if self.require_prompt_empty and not wrapper.is_prompt_empty():
            return GateResult(False, "prompt_has_text")
        if self.require_output_stable_seconds > 0 and not wrapper.is_output_stable(self.require_output_stable_seconds):
            return GateResult(False, "output_unstable")
        return GateResult(True, "ok")


@dataclass(frozen=True, slots=True)
class RetryPolicy:
    """Bounded retry schedule when delivery is pending but unsafe."""

    initial: float = INITIAL_RETRY_DELAY
    maximum: float = 2.0
    multiplier: float = 2.0

    def delay(self, attempt: int, *, pending_for: float | None = None) -> float:
        if attempt <= 0:
            return 0.0
        d = self.initial * (self.multiplier ** (attempt - 1))
        return min(self.maximum, d)


@dataclass(frozen=True, slots=True)
class TwoPhaseRetryPolicy:
    """Retry policy with a warm phase and a slower phase.

    Keeps retry maximum low for the first N seconds after messages become pending,
    then allows a higher maximum (lower overhead) if the tool stays unsafe.
    """

    initial: float = INITIAL_RETRY_DELAY
    multiplier: float = 2.0
    warm_maximum: float = 2.0
    warm_seconds: float = 60.0
    cold_maximum: float = 5.0

    def delay(self, attempt: int, *, pending_for: float | None = None) -> float:
        if attempt <= 0:
            return 0.0
        d = self.initial * (self.multiplier ** (attempt - 1))
        max_delay = self.warm_maximum
        if pending_for is not None and pending_for >= self.warm_seconds:
            max_delay = self.cold_maximum
        return min(max_delay, d)


@dataclass(frozen=True)
class PTYToolConfig:
    """Tool-specific configuration for PTY integration.

    Captures all differences between Claude, Gemini, and Codex PTY modes
    to enable a unified run_pty_with_hcom() runner.

    Attributes:
        tool: Tool identifier ("claude", "gemini", "codex")
        ready_pattern: Screen pattern indicating PTY is ready for input
        gate: DeliveryGate configuration for injection safety
        verify_timeout: Seconds for overall verify phase (default 10s)
        retry: Retry policy for failed deliveries
        is_idle_fn: Optional idle check (None = skip idle check for codex)
        on_ready_fn: Optional callback when PTY ready (e.g., start transcript watcher)
        build_inject_text: Function to build injection text from instance name
        start_pending: Whether to start delivery loop with pending=True
        use_termux_bypass: Apply termux_shebang_bypass to command
        extra_env: Additional env vars for runner script (e.g., HCOM_PTY_MODE)
    """

    tool: str
    ready_pattern: bytes
    gate: DeliveryGate
    verify_timeout: float = VERIFY_CURSOR_TIMEOUT
    retry: RetryPolicyProtocol = TwoPhaseRetryPolicy()
    is_idle_fn: Callable[[str], bool] | None = None
    on_ready_fn: Callable[[str, list[bool]], None] | None = None
    build_inject_text: Callable[[str], str] | None = None
    build_inject_text_with_hint: Callable[[str], str] | None = None  # Retry hint (Codex)
    start_pending: bool = True
    use_termux_bypass: bool = False
    extra_env: dict[str, str] | None = None


def _get_instance_status_context(instance_name: str) -> tuple[str | None, str | None]:
    """Get (status, context) from DB for status callbacks."""
    if not instance_name:
        return None, None
    try:
        from ..core.db import get_db

        row = (
            get_db()
            .execute(
                "SELECT status, status_context FROM instances WHERE name = ?",
                (instance_name,),
            )
            .fetchone()
        )
        if row:
            return row["status"], row["status_context"]
        return None, None
    except Exception:
        return None, None


def _update_gate_block_status(
    instance_name: str,
    reason: str,
    block_since: float | None,
    current_status: str | None,
    current_context: str | None,
) -> float:
    """Update status when gate blocks delivery for 2+ seconds.

    Only updates if current status is "listening" - don't overwrite active/blocked.
    Returns updated block_since timestamp.

    Uses set_gate_status() for tui:* contexts (no event logged) to avoid event bloat.
    Uses set_status() for pty:approval (blocked) which is a significant state change.
    """
    now = time.monotonic()
    if block_since is None:
        return now

    # Only update status if instance is currently listening
    # Don't overwrite active/blocked status
    if current_status != "listening":
        return block_since

    # After 2 seconds of blocking, log the reason
    if (now - block_since) >= 2.0:
        # Use hyphens in context to match existing tui:not-ready format
        reason_formatted = reason.replace("_", "-")

        # Approval waiting = blocked status (consistent with Claude hooks)
        # This uses set_status() because blocked is a significant state change
        if reason_formatted == "approval":
            if current_context != "pty:approval":
                from ..core.instances import set_status

                set_status(
                    instance_name,
                    "blocked",
                    context="pty:approval",
                    detail="waiting for user approval",
                )
        else:
            # tui:* contexts use set_gate_status() - no event logged
            context = f"tui:{reason_formatted}"
            if current_context != context:
                from ..core.instances import set_gate_status

                detail_map = {
                    "not-idle": "waiting for idle status",
                    "user-active": "user is typing",
                    "not-ready": "prompt not visible",
                    "output-unstable": "output still streaming",
                    "prompt-has-text": "uncommitted text in prompt",
                }
                set_gate_status(
                    instance_name,
                    context=context,
                    detail=detail_map.get(reason_formatted, reason),
                )
    return block_since


def _clear_gate_block_status(instance_name: str, current_status: str | None, current_context: str | None) -> None:
    """Clear gate block status after successful delivery.

    Uses set_gate_status() for tui:* contexts (no event logged).
    Uses set_status() for pty:approval since transitioning from blocked is significant.
    """
    # Clear listening with tui: context (no event logged)
    if current_status == "listening" and current_context and current_context.startswith("tui:"):
        from ..core.instances import set_gate_status

        set_gate_status(instance_name, context="", detail="")
    # Clear blocked with pty:approval context (Codex approval cleared) - logs event
    elif current_status == "blocked" and current_context == "pty:approval":
        from ..core.instances import set_status

        set_status(instance_name, "listening", context="ready")


def run_notify_delivery_loop(
    *,
    running: Callable[[], bool],
    notifier: Notifier,
    wrapper: PTYLike,
    has_pending: Callable[[], bool],
    try_deliver_text: Callable[[], str | None],
    try_deliver_text_with_hint: Callable[[], str | None] | None = None,  # Retry fresh injects (Codex)
    try_enter: Callable[[], bool] | None = None,
    get_pending_snapshot: Callable[[], tuple[int, ...]] | None = None,
    is_idle: Callable[[], bool] | None = None,
    gate: DeliveryGate,
    retry: RetryPolicyProtocol = RetryPolicy(),
    idle_wait: float = 30.0,
    start_pending: bool = False,
    instance_name: str = "",
    get_cursor: Callable[[], int] | None = None,
    verify_timeout: float = 10.0,  # 10s for slow systems
    # Optional: skip all status callbacks (Rust mode)
    skip_status_callbacks: bool = False,
    # Custom callbacks (None = use default, skip_status_callbacks=True overrides)
    on_gate_blocked: Callable[[str, str, float | None], float] | None = None,
    on_delivered: Callable[[str], None] | None = None,
) -> None:
    """Run a notify-driven delivery loop with delivery verification.

    Design goals:
    - Zero periodic DB polling when there are no pending messages.
    - Delivery attempts happen only after a wake event or bounded retry tick.
    - When unsafe (not at prompt, user typing, approval), retry backs off.
    - Verify delivery via cursor advance (hook reads messages, advances cursor).

    States:
    - idle: no pending messages, sleeping on notifier
    - pending: messages exist, waiting for safe gate to inject
    - wait_text_render: text-only injected, waiting for text to appear
    - wait_text_clear: Enter sent, waiting for text to clear
    - verify_cursor: waiting for cursor advance to confirm delivery (if enabled)

    Args:
        get_cursor: Callback returning current cursor position (last_event_id).
            If provided, enables cursor verification. If None, Phase 3 is skipped
            and delivery succeeds once Phase 2 clears the input box.
        verify_timeout: Max seconds for overall verify phase (default 10s).
        try_deliver_text: Inject text only, returns text if sent.
        try_deliver_text_with_hint: Inject text only with hint on retry (Codex).
        try_enter: Inject just Enter key (used for retry when text already injected).
        get_pending_snapshot: Optional callable returning tuple of pending message IDs.
        skip_status_callbacks: If True, skip all status updates (Rust mode).
        on_gate_blocked: Custom callback when gate blocks. None = use default DB updates.
        on_delivered: Custom callback after delivery. None = use default DB updates.

    Verify phase:
        Phase 1: inject text only, wait for text to appear in input box
        Phase 2: send Enter, wait for text to clear (retry Enter with backoff)
        Phase 3: verify cursor advance (delivery confirmed). If cursor tracking fails,
                 after a few retries we check pending state to decide success vs failure.
    """

    # Default status callbacks use helper functions
    use_default_gate_blocked = not skip_status_callbacks and on_gate_blocked is None
    use_default_delivered = not skip_status_callbacks and on_delivered is None

    def _noop_gate_blocked(_inst: str, _reason: str, block_since: float | None) -> float:
        return block_since if block_since is not None else time.monotonic()

    def _noop_delivered(_inst: str) -> None:
        pass

    # For custom callbacks or skip mode
    resolved_on_gate_blocked = _noop_gate_blocked if skip_status_callbacks else (on_gate_blocked or _noop_gate_blocked)
    resolved_on_delivered = _noop_delivered if skip_status_callbacks else (on_delivered or _noop_delivered)

    # Phase timeouts
    phase1_timeout = 2.0
    phase2_timeout = 2.0
    max_enter_attempts = 3
    enter_backoff_base = 0.2

    # State: 'idle' | 'pending' | 'wait_text_render' | 'wait_text_clear' | 'verify_cursor'
    state = "pending" if start_pending else "idle"
    attempt = 0
    pending_since: float | None = time.monotonic() if start_pending else None
    block_since: float | None = None

    # Injection/verification state
    cursor_before: int = 0
    phase_started_at: float = 0.0
    injected_text: str = ""
    inject_attempt: int = 0
    enter_attempt: int = 0
    last_pending_snapshot: tuple[int, ...] | None = None
    failed_snapshot: tuple[int, ...] | None = None

    # Log debounce state
    log_state = DeliveryLoopState()

    def _is_idle() -> bool:
        return is_idle() if is_idle is not None else True

    def _log_state(new_state: str, reason: str = "") -> None:
        """Log state transition for debugging."""
        from ..core.log import log_info

        log_info(
            "pty",
            "delivery.state",
            state=new_state,
            reason=reason,
            instance=instance_name,
        )

    def _pending_state() -> tuple[bool, tuple[int, ...] | None]:
        if get_pending_snapshot is not None:
            try:
                snapshot = get_pending_snapshot()
                return bool(snapshot), snapshot
            except Exception:
                return has_pending(), None
        return has_pending(), None

    def _maybe_reset_attempts(snapshot: tuple[int, ...] | None) -> None:
        nonlocal inject_attempt, failed_snapshot, last_pending_snapshot
        if snapshot is None:
            return
        if last_pending_snapshot is None:
            last_pending_snapshot = snapshot
            return
        if snapshot != last_pending_snapshot:
            inject_attempt = 0
            failed_snapshot = None
            last_pending_snapshot = snapshot

    def _mark_delivery_failed(snapshot: tuple[int, ...] | None) -> None:
        nonlocal failed_snapshot
        failed_snapshot = snapshot
        if instance_name:
            try:
                from ..core.instances import set_gate_status

                set_gate_status(
                    instance_name,
                    context="tui:delivery-failed",
                    detail="delivery failed; check terminal",
                )
            except Exception:
                pass

    def _handle_delivered() -> None:
        """Handle success: clear gate status and reset state."""
        nonlocal state, pending_since, attempt, block_since, inject_attempt, enter_attempt
        if instance_name:
            if use_default_delivered:
                status, context = _get_instance_status_context(instance_name)
                _clear_gate_block_status(instance_name, status, context)
            elif resolved_on_delivered:
                resolved_on_delivered(instance_name)
        state = "pending" if has_pending() else "idle"
        if state == "idle":
            pending_since = None
        attempt = 0
        block_since = None
        inject_attempt = 0
        enter_attempt = 0

    try:
        while running():
            # === IDLE STATE ===
            if state == "idle":
                notifier.wait(timeout=idle_wait)
                if not running():
                    break
                pending, snapshot = _pending_state()
                _maybe_reset_attempts(snapshot)
                if pending:
                    state = "pending"
                    pending_since = time.monotonic()
                    _log_state("pending", "messages_arrived")
                continue

            # === PHASE 1: WAIT FOR TEXT TO APPEAR ===
            if state == "wait_text_render":
                elapsed = time.monotonic() - phase_started_at
                if elapsed > phase1_timeout:
                    from ..core.log import log_warn

                    log_warn(
                        "pty",
                        "injection.phase1.timeout",
                        instance=instance_name,
                        extracted=wrapper.get_input_box_text() or "None",
                        attempt=inject_attempt,
                    )
                    state = "pending"
                    inject_attempt += 1
                    attempt += 1
                    _log_state("pending", "phase1_timeout")
                    continue

                text = wrapper.get_input_box_text()
                if text is not None and injected_text and injected_text in text:
                    from ..core.log import log_info

                    log_info(
                        "pty",
                        "injection.phase1.text_appeared",
                        instance=instance_name,
                        elapsed_ms=int(elapsed * 1000),
                        extracted_text=text[:40],
                    )
                    state = "wait_text_clear"
                    phase_started_at = time.monotonic()
                    enter_attempt = 0
                    if try_enter is not None and not wrapper.is_user_active() and not wrapper.is_waiting_approval():
                        try_enter()
                        log_info(
                            "pty",
                            "injection.phase2.enter_sent",
                            instance=instance_name,
                            enter_attempt=enter_attempt,
                        )
                else:
                    time.sleep(0.01)
                continue

            # === PHASE 2: WAIT FOR TEXT TO CLEAR ===
            if state == "wait_text_clear":
                elapsed = time.monotonic() - phase_started_at
                text = wrapper.get_input_box_text()
                if text == "":
                    from ..core.log import log_info

                    log_info(
                        "pty",
                        "injection.phase2.text_cleared",
                        instance=instance_name,
                        elapsed_ms=int(elapsed * 1000),
                    )
                    if get_cursor is None:
                        _handle_delivered()
                        continue
                    state = "verify_cursor"
                    phase_started_at = time.monotonic()
                    _log_state("verify_cursor", "text_cleared")
                    continue

                if elapsed > phase2_timeout:
                    if enter_attempt < max_enter_attempts:
                        if try_enter is not None and not wrapper.is_user_active() and not wrapper.is_waiting_approval():
                            from ..core.log import log_info

                            backoff = enter_backoff_base * (2**enter_attempt)
                            try_enter()
                            log_info(
                                "pty",
                                "injection.phase2.retry_enter",
                                instance=instance_name,
                                enter_attempt=enter_attempt + 1,
                                backoff_ms=int(backoff * 1000),
                                text_still_present=(text or "")[:40],
                            )
                            enter_attempt += 1
                            phase_started_at = time.monotonic()
                            time.sleep(backoff)
                            continue
                    from ..core.log import log_warn

                    log_warn(
                        "pty",
                        "injection.phase2.timeout",
                        instance=instance_name,
                        enter_attempt=enter_attempt,
                        extracted=text or "None",
                    )
                    state = "pending"
                    inject_attempt += 1
                    attempt += 1
                    _log_state("pending", "phase2_timeout")
                    continue

                time.sleep(0.01)
                continue

            # === PHASE 3: VERIFY CURSOR ADVANCE ===
            if state == "verify_cursor":
                elapsed = time.monotonic() - phase_started_at
                if get_cursor is not None:
                    current_cursor = get_cursor()
                    if current_cursor > cursor_before:
                        _handle_delivered()
                        _log_state(
                            state,
                            "cursor_advanced_more_messages" if state == "pending" else "cursor_advanced_delivered",
                        )
                        continue

                if elapsed > verify_timeout:
                    from ..core.log import log_error

                    inject_attempt += 1
                    log_error(
                        "pty",
                        "injection.phase3.timeout",
                        "Cursor verification timeout",
                        instance=instance_name,
                        inject_attempt=inject_attempt,
                        cursor_before=cursor_before,
                        cursor_after=get_cursor() if get_cursor else 0,
                    )

                    if inject_attempt < 3:
                        state = "pending"
                        attempt += 1
                        _log_state("pending", "phase3_timeout_retry")
                        continue

                    pending, snapshot = _pending_state()
                    if not pending:
                        _handle_delivered()
                        _log_state("idle", "cursor_timeout_pending_cleared")
                        continue

                    log_error(
                        "pty",
                        "injection.failed",
                        "Message injection failed after multiple attempts",
                        instance=instance_name,
                        message_ids=list(snapshot or ()),
                        total_attempts=inject_attempt,
                        last_phase="verify_cursor",
                    )
                    _mark_delivery_failed(snapshot)
                    state = "pending"
                    attempt = 0
                    _log_state("pending", "delivery_failed")
                    continue

                time.sleep(0.01)
                continue

            # === PENDING STATE ===
            # Check if still pending before attempting delivery
            pending, snapshot = _pending_state()
            _maybe_reset_attempts(snapshot)
            if not pending:
                state = "idle"
                pending_since = None
                attempt = 0
                block_since = None
                _log_state("idle", "no_pending_messages")
                continue

            if snapshot is not None and failed_snapshot == snapshot:
                # Failed batch: wait for new messages to reset attempts
                notifier.wait(timeout=idle_wait)
                continue

            result = gate.evaluate(wrapper=wrapper, is_idle=_is_idle(), instance_name=instance_name)
            if result.safe:
                # Snapshot cursor before injection (if verification enabled)
                if get_cursor is not None:
                    cursor_before = get_cursor()

                # Recheck has_pending() immediately before injection to reduce race window
                # where hooks may have delivered messages between gate check and injection
                pending, snapshot = _pending_state()
                _maybe_reset_attempts(snapshot)
                if not pending:
                    state = "idle"
                    pending_since = None
                    attempt = 0
                    block_since = None
                    _log_state("idle", "no_pending_after_gate")
                    continue

                # Use hint version only after a failed inject attempt
                if inject_attempt > 0 and try_deliver_text_with_hint is not None:
                    injected = try_deliver_text_with_hint()
                else:
                    injected = try_deliver_text()
                if injected:
                    from ..core.log import log_info

                    injected_text = injected
                    phase_started_at = time.monotonic()
                    enter_attempt = 0
                    state = "wait_text_render"
                    log_info(
                        "pty",
                        "injection.phase1.start",
                        instance=instance_name,
                        text_preview=injected_text[:40],
                        attempt=inject_attempt,
                    )
                    _log_state("wait_text_render", "text_injected")
                    continue
                attempt += 1
            else:
                # Gate blocked - log with debounce and update TUI after 2 seconds
                _log_gate_block(log_state, result.reason, instance_name)
                current_status: str | None = None
                current_context: str | None = None
                if instance_name:
                    # Fetch status once - reused for callback AND recovery logic
                    if use_default_gate_blocked or result.reason == "not_idle":
                        current_status, current_context = _get_instance_status_context(instance_name)
                    # Call status callback
                    if use_default_gate_blocked:
                        block_since = _update_gate_block_status(
                            instance_name,
                            result.reason,
                            block_since,
                            current_status,
                            current_context,
                        )
                    elif resolved_on_gate_blocked:
                        block_since = resolved_on_gate_blocked(instance_name, result.reason, block_since)
                    # Stability-based recovery: if status stuck "active" but output stable 10s,
                    # assume ESC cancelled or similar - flip to listening
                    if result.reason == "not_idle" and current_status == "active" and wrapper.is_output_stable(10.0):
                        from ..core.instances import set_status
                        from ..core.log import log_info as _log_info

                        set_status(instance_name, "listening", context="pty:recovered")
                        _log_info(
                            "pty",
                            "status.recovered",
                            instance=instance_name,
                            reason="stable_10s",
                        )
                        attempt = 0  # Reset attempts, re-evaluate immediately
                        continue
                attempt += 1

            # Pending but couldn't deliver: wait for retry
            pending_for = (time.monotonic() - pending_since) if pending_since is not None else None
            delay = retry.delay(attempt, pending_for=pending_for)
            if delay <= 0:
                continue
            notified = notifier.wait(timeout=delay)
            # If notified, snap back to fast retries
            if notified:
                attempt = 0
            if not running():
                break

            # Re-check if still pending
            pending, snapshot = _pending_state()
            _maybe_reset_attempts(snapshot)
            if not pending:
                state = "idle"
                attempt = 0
                pending_since = None
                block_since = None
    finally:
        try:
            notifier.close()
        except Exception:
            pass


class NotifyServerAdapter:
    """Adapter for pty.pty_common.NotifyServer to satisfy Notifier Protocol."""

    def __init__(self) -> None:
        from .pty_common import NotifyServer

        self._notify = NotifyServer()
        self.port: int | None = None

    def start(self) -> bool:
        ok = self._notify.start()
        self.port = self._notify.port
        return ok

    def wait(self, *, timeout: float) -> bool:
        return self._notify.wait(timeout=timeout)

    def close(self) -> None:
        self._notify.close()


__all__ = [
    "DeliveryGate",
    "GateResult",
    "Notifier",
    "NotifyServerAdapter",
    "PTYLike",
    "PTYToolConfig",
    "RetryPolicy",
    "RetryPolicyProtocol",
    "TwoPhaseRetryPolicy",
    "run_notify_delivery_loop",
]
