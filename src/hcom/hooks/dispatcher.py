"""Hook dispatcher - single entry point with clean parent/subagent separation"""
from __future__ import annotations
from typing import Any
import sys
import os
import json
import re

from ..core.paths import ensure_hcom_directories
from ..core.instances import load_instance_position
from ..core.db import get_db, find_instance_by_session
from . import subagent, parent
from .utils import init_hook_context, log_hook_error


def _auto_approve_hcom_bash(hook_data: dict[str, Any]) -> None:
    """Auto-approve safe hcom bash commands (fast path, no instance needed).

    Allows vanilla instances to run 'hcom start' and disabled instances to re-enable.
    Exits if approved.
    """
    tool_name = hook_data.get('tool_name', '')
    if tool_name != 'Bash':
        return

    tool_input = hook_data.get('tool_input', {})
    command = tool_input.get('command', '')
    if not command:
        return

    from ..shared import HCOM_COMMAND_PATTERN
    from .utils import is_safe_hcom_command

    matches = list(re.finditer(HCOM_COMMAND_PATTERN, command))
    if matches and is_safe_hcom_command(command):
        output = {
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "allow"
            }
        }
        print(json.dumps(output, ensure_ascii=False))
        sys.exit(0)


def handle_hook(hook_type: str) -> None:
    """Hook dispatcher with clean parent/subagent separation

    Error handling strategy:
    - Non-participants (no instance / disabled): exit 0 silently to avoid leaking errors
      into normal Claude Code usage when user has hcom installed but not using it
    - Participants (enabled): errors surface normally
    """
    # The try/except below is commented out during development to see errors.
    # In production, it catches pre-gate errors (before we know if instance exists/enabled).

    # try:
    _handle_hook_impl(hook_type)
    # except Exception as e:
    #     # Pre-gate error (before instance context resolved) - must be silent
    #     # because we don't know if user is even using hcom
    #     log_hook_error(f'handle_hook:{hook_type}', e)
    #     sys.exit(0)

def _handle_hook_impl(hook_type: str) -> None:
    """Hook dispatcher implementation"""

    # ============ SETUP, LOAD, AUTO-APPROVE, SYNC (BOTH CONTEXTS) ============

    hook_data = json.load(sys.stdin)
    tool_name = hook_data.get('tool_name', '')

    # Get real session_id from CLAUDE_ENV_FILE path (workaround for CC fork bug)
    # CC passes wrong session_id in hook_data for --fork-session scenarios
    from .parent import get_real_session_id
    env_file = os.environ.get('CLAUDE_ENV_FILE')
    session_id = get_real_session_id(hook_data, env_file)

    # Store corrected session_id back into hook_data for downstream functions
    hook_data['session_id'] = session_id

    if not ensure_hcom_directories():
        log_hook_error('handle_hook', Exception('Failed to create directories'))
        sys.exit(0)

    get_db()

    if hook_type == 'pre':
        _auto_approve_hcom_bash(hook_data)

    # ============ TASK TRANSITIONS (PARENT CONTEXT) ============

    # Task start - enter subagent context
    if hook_type == 'pre' and tool_name == 'Task':
        parent.start_task(session_id, hook_data)
        sys.exit(0)

    # Task end (completed) - exit subagent context
    if hook_type == 'post' and tool_name == 'Task':
        parent.end_task(session_id, hook_data, interrupted=False)
        sys.exit(0)

    # Task end (interrupted) - exit subagent context
    if subagent.in_subagent_context(session_id) and hook_type == 'userpromptsubmit':
        parent.end_task(session_id, hook_data, interrupted=True)
        # Fall through to parent handling

    # ============ SUBAGENT INSTANCE HOOKS ============

    # subagent gate 
    if subagent.in_subagent_context(session_id):

        match hook_type:
            case 'subagent-start':
                agent_id = hook_data.get('agent_id')
                agent_type = hook_data.get('agent_type')
                subagent.track_subagent(session_id, agent_id, agent_type)
                subagent.subagent_start(hook_data)
                sys.exit(0)
            case 'subagent-stop':
                subagent.subagent_stop(hook_data)
                sys.exit(0)
            case 'post':
                subagent.posttooluse(hook_data, '', None)
                sys.exit(0)

    # ============  PARENT INSTANCE HOOKS ============
    else:
        
        if hook_type == 'sessionstart':
            parent.sessionstart(hook_data)
            sys.exit(0)

        # Resolve instance for parent hooks
        instance_name, updates, is_matched_resume = init_hook_context(hook_data, hook_type)
        instance_data = load_instance_position(instance_name)

        # exists gate
        if not instance_data:
            sys.exit(0)

        # Status-only for stop pending (disabled but awaiting exit)
        if not instance_data.get('enabled') and instance_data.get('external_stop_pending'):
            parent.handle_stop_pending(hook_type, hook_data, instance_name, instance_data)
            sys.exit(0)

        # enabled gate
        if not instance_data.get('enabled', False):
            sys.exit(0)

        match hook_type:
            case 'pre':
                parent.pretooluse(hook_data, instance_name, tool_name)
            case 'post':
                parent.posttooluse(hook_data, instance_name, instance_data, updates)
            case 'poll':
                parent.stop(instance_name, instance_data)
            case 'notify':
                parent.notify(hook_data, instance_name, updates, instance_data)
            case 'userpromptsubmit':
                parent.userpromptsubmit(hook_data, instance_name, updates, is_matched_resume, instance_data)
            case 'sessionend':
                parent.sessionend(hook_data, instance_name, updates)

    sys.exit(0)
