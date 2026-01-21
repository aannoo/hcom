"""Unified PTY handler for hcom tool integrations.

This module consolidates the common PTY logic across Claude, Gemini, and Codex,
reducing duplication while preserving tool-specific behaviors through PTYToolConfig.

Tool-specific modules (claude.py, gemini.py, codex.py) become thin wrappers
that call the unified functions here.
"""

from __future__ import annotations

import base64
import json
import os
import random
import shlex
import signal
import sys
import threading
import typing
from pathlib import Path

from .pty_wrapper import PTYWrapper
from .push_delivery import (
    DeliveryGate,
    NotifyServerAdapter,
    PTYLike,
    PTYToolConfig,
    TwoPhaseRetryPolicy,
    run_notify_delivery_loop,
)
from .pty_common import (
    inject_message as _pty_inject,
    inject_enter as _inject_enter,
    get_instance_cursor,
    get_instance_status,
    wait_for_process_registration,
    register_notify_port,
    set_terminal_title,
    GateWrapperView,
    HeartbeatNotifier,
    create_sighup_handler,
    build_message_preview,
    build_listen_instruction,
    termux_shebang_bypass,
    GEMINI_READY_PATTERN,
    CLAUDE_CODEX_READY_PATTERN,
    STATUS_CONTEXT_EXIT_KILLED,
    STATUS_CONTEXT_EXIT_CLOSED,
)
from ..core.log import log_info, log_warn, log_error
from ..core.timeouts import PTY_VERIFY_TIMEOUT, OUTPUT_STABLE_SECONDS


# ==================== Idle Checkers ====================


def _claude_is_idle(instance_name: str) -> bool:
    """Claude idle check: direct status comparison."""
    status, _ = get_instance_status(instance_name)
    return status == "listening"


def _gemini_is_idle(instance_name: str) -> bool:
    """Gemini idle check: direct status comparison.

    Previously used DebouncedIdleChecker (0.4s debounce) because AfterAgent fired
    multiple times per turn during tool loops. However, Gemini CLI commit 15c9f88da
    (Dec 2025) fixed the underlying skipNextSpeakerCheck bug - AfterAgent now fires
    consistently after processTurn completes, making debouncing unnecessary.

    See: dev/gemini-afteragent-detection-findings.md
         dev/gemini/afteragent-bug.md
         src/hcom/tools/gemini/settings.py (skipNextSpeakerCheck comment)
    """
    status, _ = get_instance_status(instance_name)
    return status == "listening"


# Codex: no idle check (is_idle_fn=None)


# ==================== Inject Text Builders ====================


def _claude_inject_text(instance_name: str) -> str:  # noqa: ARG001
    """Claude: inject <hcom> trigger only.

    Hook shows full message to human via system message in TUI + separate JSON for agent.
    Minimal trigger sufficient since hook handles both human and agent presentation.
    """
    del instance_name  # Unused but required for signature compatibility
    return "<hcom>"


def _gemini_inject_text(instance_name: str) -> str:
    """Gemini: inject message preview for human visibility.

    Hook only shows JSON to agent (no human-visible system message like Claude).
    Preview in terminal gives human context since hook output is agent-only.
    BeforeAgent hook still delivers full message to agent via additionalContext.
    """
    return build_message_preview(instance_name)


def _codex_inject_text(instance_name: str) -> str:
    """Codex: inject message preview + hcom listen instruction.

    Preview shows human message context in terminal (like Gemini).
    Bash command output truncated for agent only (command execution-based delivery).
    No BeforeAgent-style hook exists - Codex executes 'hcom listen' as shell command.
    """
    return build_listen_instruction(instance_name)


def _codex_inject_text_with_hint(instance_name: str) -> str:
    """Codex: inject with explicit hint after a failed inject."""
    return f"{build_listen_instruction(instance_name)} | Run: hcom listen"


# ==================== Tool Configurations ====================


def _get_default_retry() -> TwoPhaseRetryPolicy:
    """Standard retry policy for all tools."""
    return TwoPhaseRetryPolicy(
        initial=0.25,
        multiplier=2.0,
        warm_maximum=2.0,
        warm_seconds=60.0,
        cold_maximum=5.0,
    )


# Gate configurations per tool
#
# Claude: require_ready_prompt=False because status bar hides in accept-edits mode.
#         require_prompt_empty=True parses ❯ prompt to distinguish placeholder vs user text.
CLAUDE_GATE = DeliveryGate(
    require_idle=True,
    require_ready_prompt=False,
    require_prompt_empty=True,
    require_output_stable_seconds=OUTPUT_STABLE_SECONDS,
    block_on_user_activity=True,
    block_on_approval=True,
)

# Gemini: require_ready_prompt=True because "Type your message" placeholder disappears when user types.
#         Pattern visibility indicates empty prompt, no separate empty check needed.
GEMINI_GATE = DeliveryGate(
    require_idle=True,
    require_ready_prompt=True,
    require_output_stable_seconds=OUTPUT_STABLE_SECONDS,
    block_on_user_activity=True,
    block_on_approval=True,
)

# Codex: require_idle=True but is_idle_fn=None (see TOOL_CONFIGS below).
#        Status tracking is reliable (hook sets idle and active is set within ~5s from transcript watcher) so it's redundant anyway.
#        output_stable + user_activity + approval already cover idle state. TODO: look at this later
CODEX_GATE = DeliveryGate(
    require_idle=True,
    require_ready_prompt=True,
    require_output_stable_seconds=OUTPUT_STABLE_SECONDS,
    block_on_user_activity=True,
    block_on_approval=True,
)

# Tool configurations
TOOL_CONFIGS: dict[str, PTYToolConfig] = {
    "claude": PTYToolConfig(
        tool="claude",
        ready_pattern=CLAUDE_CODEX_READY_PATTERN,
        gate=CLAUDE_GATE,
        verify_timeout=PTY_VERIFY_TIMEOUT,
        retry=_get_default_retry(),
        is_idle_fn=_claude_is_idle,
        build_inject_text=_claude_inject_text,
        start_pending=True,
        use_termux_bypass=False,
        extra_env={"HCOM_PTY_MODE": "1"},
    ),
    "gemini": PTYToolConfig(
        tool="gemini",
        ready_pattern=GEMINI_READY_PATTERN,
        gate=GEMINI_GATE,
        verify_timeout=PTY_VERIFY_TIMEOUT,
        retry=_get_default_retry(),
        is_idle_fn=_gemini_is_idle,
        build_inject_text=_gemini_inject_text,
        start_pending=True,
        use_termux_bypass=True,
    ),
    "codex": PTYToolConfig(
        tool="codex",
        ready_pattern=CLAUDE_CODEX_READY_PATTERN,
        gate=CODEX_GATE,
        verify_timeout=PTY_VERIFY_TIMEOUT,
        retry=_get_default_retry(),
        is_idle_fn=None,  # Codex: skip idle check
        build_inject_text=_codex_inject_text,
        build_inject_text_with_hint=_codex_inject_text_with_hint,  # Retry hint
        start_pending=False,  # Codex: don't start pending
        use_termux_bypass=True,
    ),
}


# ==================== Unified Receiver Thread ====================


def _run_receiver_thread(
    *,
    config: PTYToolConfig,
    process_id: str,
    instance_name: str,
    gate_wrapper: GateWrapperView,
    running_flag: list[bool],
) -> None:
    """Unified receiver thread using push_delivery loop.

    Works for all tools with behavior controlled by PTYToolConfig.
    """
    from ..core.db import get_instance
    from ..core.messages import get_unread_messages

    current = {"name": instance_name}

    # Wait for process registration (hcom-launched instances)
    bound_name, instance = wait_for_process_registration(
        process_id,
        timeout=30,
        require_session_id=False,
    )
    if not instance or not bound_name:
        log_error(
            "pty",
            "pty.exit",
            "process binding not found",
            instance=current["name"],
            tool=config.tool,
        )
        return

    current["name"] = bound_name
    set_terminal_title(current["name"])

    # Setup notify server
    notifier = NotifyServerAdapter()
    if not notifier.start():
        log_warn("pty", "notify.fail", instance=current["name"], tool=config.tool)

    register_notify_port(current["name"], notifier.port, notifier.port is not None)

    def _refresh_binding() -> bool:
        """Refresh binding from DB. Returns True if binding is valid."""
        from ..core.db import get_process_binding, migrate_notify_endpoints

        binding = get_process_binding(process_id)
        if not binding:
            return False
        new_name = binding.get("instance_name")
        if new_name and new_name != current["name"]:
            migrate_notify_endpoints(current["name"], new_name)
            current["name"] = new_name
            set_terminal_title(current["name"])
        register_notify_port(current["name"], notifier.port, notifier.port is not None)
        return True

    def running() -> bool:
        return bool(running_flag[0])

    def is_idle() -> bool:
        """Check idle status using config's is_idle_fn."""
        if not _refresh_binding():
            return False
        if config.is_idle_fn is None:
            return True  # No idle check = always idle (codex)
        return config.is_idle_fn(current["name"])

    def get_cursor() -> int:
        """Get current cursor position for delivery verification."""
        _refresh_binding()
        return get_instance_cursor(current["name"])

    def has_pending() -> bool:
        """Check for unread messages."""
        return bool(get_pending_snapshot())

    def get_pending_snapshot() -> tuple[int, ...]:
        """Return pending message IDs for retry tracking."""
        if not _refresh_binding():
            return ()
        inst = get_instance(current["name"])
        if not inst:
            return ()
        messages, _ = get_unread_messages(current["name"], update_position=False)
        ids: list[int] = []
        for message in messages:
            msg_id = message.get("event_id") or message.get("id")
            if isinstance(msg_id, int):
                ids.append(msg_id)
        return tuple(ids)

    def _fit_injection_text(text: str) -> str:
        """Ensure injected text fits in a single line."""
        terminal_width = gate_wrapper.get_screen_columns()
        input_box_width = max(10, terminal_width - 15)
        if len(text) <= input_box_width:
            return text
        return "<hcom>"

    def try_deliver_text() -> str | None:
        """Inject text only (no Enter)."""
        if not _refresh_binding():
            return None
        if not gate_wrapper.actual_port:
            return None
        inst = get_instance(current["name"])
        if not inst:
            return None
        if config.build_inject_text is None:
            return None
        text = _fit_injection_text(config.build_inject_text(current["name"]))
        ok = _pty_inject(gate_wrapper.actual_port, text, submit=False)
        if not ok:
            log_warn("pty", "inject.fail", instance=current["name"], tool=config.tool)
            return None
        return text

    def try_enter() -> bool:
        """Inject just Enter key (for retry when text already in buffer)."""
        if not _refresh_binding():
            return False
        if not gate_wrapper.actual_port:
            return False
        return _inject_enter(gate_wrapper.actual_port)

    def try_deliver_text_with_hint() -> str | None:
        """Inject with hint for retry fresh injects (Codex: ' | Run: hcom listen')."""
        if not _refresh_binding():
            return None
        if not gate_wrapper.actual_port:
            return None
        inst = get_instance(current["name"])
        if not inst:
            return None
        if config.build_inject_text_with_hint is None:
            # No hint configured, fall back to normal inject
            if config.build_inject_text is None:
                return None
            text = config.build_inject_text(current["name"])
        else:
            text = config.build_inject_text_with_hint(current["name"])
        text = _fit_injection_text(text)
        ok = _pty_inject(gate_wrapper.actual_port, text, submit=False)
        if not ok:
            log_warn("pty", "inject.fail", instance=current["name"], tool=config.tool)
            return None
        return text

    try:
        run_notify_delivery_loop(
            running=running,
            notifier=HeartbeatNotifier(notifier, lambda: current["name"]),
            wrapper=typing.cast(PTYLike, gate_wrapper),
            has_pending=has_pending,
            try_deliver_text=try_deliver_text,
            try_deliver_text_with_hint=(
                try_deliver_text_with_hint if config.build_inject_text_with_hint else None
            ),
            try_enter=try_enter,
            get_pending_snapshot=get_pending_snapshot,
            is_idle=is_idle if config.is_idle_fn else None,
            gate=config.gate,
            retry=config.retry,
            idle_wait=30.0,
            start_pending=config.start_pending,
            instance_name=current["name"],
            get_cursor=get_cursor,
            verify_timeout=config.verify_timeout,
        )
    except Exception as e:
        import traceback
        from ..core.instances import set_status

        tb = traceback.format_exc()
        log_error(
            "pty",
            "receiver.crash",
            str(e),
            instance=current["name"],
            tool=config.tool,
            traceback=tb,
        )
        set_status(current["name"], "error", context="pty:crash", detail=str(e)[:200])


# ==================== Unified PTY Runner ====================


def run_pty_with_hcom(
    tool: str,
    instance_name: str,
    tool_args: list[str],
    *,
    on_ready_extra: typing.Callable[[str, list[bool]], None] | None = None,
) -> int:
    """Unified PTY runner for all tools.

    Args:
        tool: Tool identifier ("claude", "gemini", "codex")
        instance_name: HCOM instance name
        tool_args: Arguments to pass to tool command
        on_ready_extra: Optional extra callback when PTY ready (e.g., transcript watcher)

    Returns:
        Exit code from tool process
    """
    config = TOOL_CONFIGS.get(tool)
    if not config:
        print(f"[pty-handler] ERROR: Unknown tool '{tool}'", file=sys.stderr)
        return 1

    process_id = os.environ.get("HCOM_PROCESS_ID")
    if not process_id:
        print(f"[{tool}-pty] ERROR: HCOM_PROCESS_ID not set", file=sys.stderr)
        return 1

    # Resolve instance name from process binding
    from ..core.db import get_process_binding

    binding = get_process_binding(process_id)
    bound_name = binding.get("instance_name") if binding else None
    if bound_name:
        instance_name = bound_name
    if not instance_name:
        print(f"[{tool}-pty] ERROR: Process binding missing", file=sys.stderr)
        return 1

    # Build command
    command = [tool, *tool_args]
    if config.use_termux_bypass:
        command = termux_shebang_bypass(command, tool)

    running_flag: list[bool] = [True]

    # Create gate_wrapper reference holder (set after wrapper creation)
    gate_wrapper_ref: list[GateWrapperView | None] = [None]

    def on_ready(port: int) -> None:
        """Callback when PTY is ready - starts receiver thread."""
        log_info(
            "pty",
            "pty.ready",
            port=port,
            instance=instance_name,
            tool=tool,
        )
        # Start receiver thread
        if gate_wrapper_ref[0] is not None:
            threading.Thread(
                target=_run_receiver_thread,
                kwargs={
                    "config": config,
                    "process_id": process_id,
                    "instance_name": instance_name,
                    "gate_wrapper": gate_wrapper_ref[0],
                    "running_flag": running_flag,
                },
                daemon=True,
            ).start()
        # Call config's on_ready callback
        if config.on_ready_fn:
            config.on_ready_fn(instance_name, running_flag)
        # Call extra on_ready callback
        if on_ready_extra:
            on_ready_extra(instance_name, running_flag)

    # Create wrapper with on_ready callback
    wrapper = PTYWrapper(
        command=command,
        instance_name=instance_name,
        tool=tool,
        port=0,
        ready_pattern=config.ready_pattern,
        on_ready=on_ready,
    )
    gate_wrapper_ref[0] = GateWrapperView(wrapper)

    # Setup SIGHUP handler
    signal.signal(
        signal.SIGHUP,
        create_sighup_handler(
            instance_name,
            running_flag,
            process_id,
            exit_context=STATUS_CONTEXT_EXIT_KILLED,
        ),
    )

    # Run - blocks until tool exits
    try:
        exit_code = wrapper.run()
        log_info(
            "pty",
            "pty.exit",
            exit_code=exit_code,
            instance=instance_name,
            tool=tool,
        )
        return exit_code
    finally:
        running_flag[0] = False
        _cleanup_pty(instance_name, process_id, tool)


def _cleanup_pty(instance_name: str, process_id: str, tool: str) -> None:
    """Clean up after PTY exit."""
    try:
        from ..core.db import get_process_binding, delete_process_binding
        from ..core.instances import set_status
        from ..core.tool_utils import stop_instance

        resolved_name = instance_name
        binding = get_process_binding(process_id)
        bound_name = binding.get("instance_name") if binding else None
        if bound_name:
            resolved_name = bound_name
        set_status(resolved_name, "inactive", context=STATUS_CONTEXT_EXIT_CLOSED)
        stop_instance(resolved_name, initiated_by="pty", reason="closed")
        delete_process_binding(process_id)
    except Exception as e:
        log_error("pty", "pty.exit", e, instance=instance_name, tool=tool)


# ==================== Unified Runner Script ====================


def create_runner_script(
    tool: str,
    cwd: str,
    instance_name: str,
    process_id: str,
    tag: str,
    tool_args: list[str],
    *,
    run_here: bool = False,
    extra_env: dict[str, str] | None = None,
    runner_module: str | None = None,
    runner_function: str | None = None,
    runner_extra_kwargs: str = "",
) -> str:
    """Create a bash script that runs a tool with hcom PTY integration.

    Args:
        tool: Tool identifier ("claude", "gemini", "codex")
        cwd: Working directory
        instance_name: HCOM instance name
        process_id: HCOM process ID
        tag: Instance tag prefix
        tool_args: Arguments to pass to tool command
        run_here: If True, script is for current terminal (no exec bash at end)
        extra_env: Additional environment variables
        runner_module: Custom module path (default: hcom.pty.pty_handler)
        runner_function: Custom function name (default: run_pty_with_hcom)
        runner_extra_kwargs: Extra kwargs string for runner function

    Returns:
        Path to created script file
    """
    from ..core.paths import hcom_path, LAUNCH_DIR

    config = TOOL_CONFIGS.get(tool)
    script_file = str(
        hcom_path(LAUNCH_DIR, f"{tool}_{instance_name}_{random.randint(1000, 9999)}.sh")
    )

    python_path = sys.executable
    module_dir = Path(__file__).parent.parent

    # Serialize args via base64
    tool_args_b64 = base64.b64encode(json.dumps(tool_args).encode()).decode()

    # For new terminal launches, keep terminal open after exit
    exec_line = (
        ""
        if run_here
        else """
# Clear identity env vars to prevent reuse after exit
unset HCOM_PROCESS_ID HCOM_LAUNCHED HCOM_PTY_MODE HCOM_TAG HCOM_CODEX_SANDBOX_MODE
exec bash -l"""
    )

    # Export HCOM_DIR if set
    hcom_dir = os.environ.get("HCOM_DIR", "")
    hcom_dir_export = f'export HCOM_DIR="{hcom_dir}"' if hcom_dir else "# HCOM_DIR not set"

    # Build env exports
    env_exports = [
        f'export HCOM_PROCESS_ID="{process_id}"',
        f'export HCOM_TAG="{tag}"',
        "export HCOM_LAUNCHED=1",
        hcom_dir_export,
    ]
    if os.environ.get("HCOM_VIA_SHIM"):
        env_exports.append("export HCOM_VIA_SHIM=1")
    if os.environ.get("HCOM_PTY_DEBUG"):
        env_exports.append("export HCOM_PTY_DEBUG=1")

    # Add tool-specific env from config
    if config and config.extra_env:
        for key, value in config.extra_env.items():
            env_exports.append(f'export {key}="{value}"')

    # Add caller-provided extra env
    if extra_env:
        for key, value in extra_env.items():
            env_exports.append(f'export {key}="{value}"')

    env_block = "\n".join(env_exports)

    # Runner module/function
    mod = runner_module or "hcom.pty.pty_handler"
    func = runner_function or "run_pty_with_hcom"
    extra_kwargs = f", {runner_extra_kwargs}" if runner_extra_kwargs else ""

    script_content = f'''#!/bin/bash
# {tool.capitalize()} hcom PTY runner ({instance_name})
cd {shlex.quote(cwd)}

unset CLAUDECODE GEMINI_CLI GEMINI_SYSTEM_MD CODEX_SANDBOX CODEX_SANDBOX_NETWORK_DISABLED CODEX_MANAGED_BY_NPM CODEX_MANAGED_BY_BUN
{env_block}

export PYTHONPATH="{module_dir.parent}:$PYTHONPATH"
{shlex.quote(python_path)} -c "
import sys, json, base64
sys.path.insert(0, '{module_dir.parent}')
from {mod} import {func}
tool_args = json.loads(base64.b64decode('{tool_args_b64}').decode())
sys.exit({func}('{tool}', '{instance_name}', tool_args{extra_kwargs}))
"{exec_line}
'''

    with open(script_file, "w") as f:
        f.write(script_content)
    os.chmod(script_file, 0o755)

    return script_file


# ==================== Unified Launch ====================


def launch_pty(
    tool: str,
    cwd: str,
    env: dict,
    instance_name: str,
    tool_args: list[str],
    *,
    tag: str = "",
    run_here: bool = False,
    extra_env: dict[str, str] | None = None,
    runner_module: str | None = None,
    runner_function: str | None = None,
    runner_extra_kwargs: str = "",
) -> str | None:
    """Launch a tool in a terminal via PTY wrapper.

    Args:
        tool: Tool identifier ("claude", "gemini", "codex")
        cwd: Working directory
        env: Environment variables dict
        instance_name: HCOM instance name
        tool_args: Arguments to pass to tool command
        tag: Instance tag prefix (optional)
        run_here: If True, run in current terminal (blocking)
        extra_env: Additional environment variables
        runner_module: Custom runner module (for tool-specific wrappers)
        runner_function: Custom runner function
        runner_extra_kwargs: Extra kwargs for runner

    Returns:
        instance_name on success, None on failure
    """
    from ..terminal import launch_terminal

    process_id = env.get("HCOM_PROCESS_ID")
    if not process_id:
        log_error(
            "pty",
            "pty.exit",
            "HCOM_PROCESS_ID not set in env",
            instance=instance_name,
            tool=tool,
        )
        return None

    script_file = create_runner_script(
        tool,
        cwd,
        instance_name,
        process_id,
        tag,
        tool_args,
        run_here=run_here,
        extra_env=extra_env,
        runner_module=runner_module,
        runner_function=runner_function,
        runner_extra_kwargs=runner_extra_kwargs,
    )

    success = launch_terminal(
        f"bash {shlex.quote(script_file)}", env, cwd=cwd, run_here=run_here
    )
    return instance_name if success else None


__all__ = [
    "TOOL_CONFIGS",
    "run_pty_with_hcom",
    "create_runner_script",
    "launch_pty",
    # Gate configs (for tool-specific overrides)
    "CLAUDE_GATE",
    "GEMINI_GATE",
    "CODEX_GATE",
]
