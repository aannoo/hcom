"""Hook utility functions"""
from __future__ import annotations
from typing import Any
from pathlib import Path
from datetime import datetime, timezone
import os
import sys
import socket
import shlex
import re

from ..core.paths import hcom_path, LOGS_DIR
from ..core.config import get_config
from ..core.instances import load_instance_position, update_instance_position
from ..core.runtime import (
    build_claude_env,
    build_hcom_bootstrap_text,
    build_launch_context,
    notify_instance,
    notify_all_instances,
)

# Platform detection
IS_WINDOWS = sys.platform == 'win32'


def log_hook_error(hook_name: str, error: Exception | str | None = None) -> None:
    """Log hook exceptions or just general logging to ~/.hcom/.tmp/logs/hooks.log for debugging"""
    import traceback
    try:
        log_file = hcom_path(LOGS_DIR) / "hooks.log"
        timestamp = datetime.now(timezone.utc).isoformat()
        if error and isinstance(error, Exception):
            tb = ''.join(traceback.format_exception(type(error), error, error.__traceback__))
            with open(log_file, 'a') as f:
                f.write(f"{timestamp}|{hook_name}|{type(error).__name__}: {error}\n{tb}\n")
        else:
            with open(log_file, 'a') as f:
                f.write(f"{timestamp}|{hook_name}|{error or 'checkpoint'}\n")
    except (OSError, PermissionError):
        pass  # Silent failure in error logging


def _detect_hcom_command_type() -> str:
    """Detect how to invoke hcom based on execution context
    Priority:
    1. uvx - If running in uv-managed Python and uvx available
           (works for both temporary uvx runs and permanent uv tool install)
    2. short - If hcom binary in PATH
    3. full - Fallback to full python invocation
    """
    import shutil
    if 'uv' in Path(sys.executable).resolve().parts and shutil.which('uvx'):
        return 'uvx'
    elif shutil.which('hcom'):
        return 'short'
    else:
        return 'full'


def _build_quoted_invocation() -> str:
    """Build invocation for fallback case - handles packages and pyz

    For packages (pip/uvx/uv tool), uses 'python -m hcom'.
    For pyz/zipapp, uses direct file path to re-invoke the same archive.
    """
    python_path = sys.executable

    # Detect if running inside a pyz/zipapp
    import zipimport
    loader = getattr(sys.modules[__name__], "__loader__", None)
    is_zipapp = isinstance(loader, zipimport.zipimporter)

    # For pyz, use __file__ path; for packages, use -m
    if is_zipapp or not __package__:
        # Standalone pyz or script - use direct file path
        script_path = str(Path(__file__).resolve())
        if IS_WINDOWS:
            py = f'"{python_path}"' if ' ' in python_path else python_path
            sp = f'"{script_path}"' if ' ' in script_path else script_path
            return f'{py} {sp}'
        else:
            return f'{shlex.quote(python_path)} {shlex.quote(script_path)}'
    else:
        # Package install (pip/uv tool/editable) - use -m
        if IS_WINDOWS:
            py = f'"{python_path}"' if ' ' in python_path else python_path
            return f'{py} -m hcom'
        else:
            return f'{shlex.quote(python_path)} -m hcom'


def build_hcom_command() -> str:
    """Build base hcom command based on execution context

    Detection always runs fresh to avoid staleness when installation method changes.
    """
    cmd_type = _detect_hcom_command_type()

    # Build command based on type
    if cmd_type == 'short':
        return 'hcom'
    elif cmd_type == 'uvx':
        return 'uvx hcom'
    else:
        # Full path fallback
        return _build_quoted_invocation()


def disable_instance(instance_name: str, initiated_by: str = 'unknown', reason: str = '') -> None:
    """Disable instance and log event
    Args:
        instance_name: Instance to disable
        initiated_by: Who initiated (from resolve_identity())
        reason: Context (e.g., 'manual', 'timeout', 'orphaned', 'external', 'stop_all', 'remote')

    Note: enabled and status are orthogonal - don't set status here.
    enabled = HCOM participation flag, status = HCOM activity state.
    Status updates happen via hooks detecting state changes and exiting properly
    (e.g., Stop hook detects enabled=false, sets inactive status, then exits).
    Setting status directly here would be lying about the actual hook/session state.
    """
    updates = {
        'enabled': False
    }

    # External stops need notification flag (instance notified via PostToolUse)
    if reason in ('stop_all', 'external', 'remote'):
        updates['external_stop_pending'] = True

    update_instance_position(instance_name, updates)
    # Notify instance to wake and see enabled=false
    notify_instance(instance_name)
    # Log all disable operations
    try:
        from ..core.db import log_event
        log_event('life', instance_name, {
            'action': 'stopped',
            'by': initiated_by,
            'reason': reason
        })
        # Push lifecycle event (rate-limited)
        from ..relay import push
        push()  # relay.py logs errors internally to relay.log
    except Exception as e:
        # Participant context - log for debugging but don't break disable
        log_hook_error('disable_instance:log_event', e)


def init_hook_context(hook_data: dict[str, Any], hook_type: str | None = None) -> tuple[str, dict[str, Any], bool]:
    """
    Initialize instance context. Flow:
    1. Resolve instance name (search by session_id, generate if not found)
    2. Build updates dict (directory, tag, session_id, mapid, background, transcript_path)
    3. Return (instance_name, updates, is_matched_resume)

    Note: Instance creation now happens at boundaries:
    - SessionStart for HCOM-launched instances
    - cmd_start for vanilla opt-in
    Auto-vivify in update_instance_position handles edge cases (mid-session reset, etc.)
    """
    from ..core.instances import resolve_instance_name
    from ..shared import MAPID

    session_id = hook_data.get('session_id', '')
    transcript_path = hook_data.get('transcript_path', '')
    tag = get_config().tag

    # Resolve instance name - existing_data is None for fresh starts
    instance_name, existing_data = resolve_instance_name(session_id, tag)

    # Build updates dict
    updates: dict[str, Any] = {
        'directory': str(Path.cwd()),
    }

    # Set tag only at creation (prevents tag field drifting from name prefix)
    if not existing_data and tag:
        updates['tag'] = tag

    # Update session_id (may have changed on resume (it would be a new instance record then, this is the same for entire instance life))
    if session_id:
        updates['session_id'] = session_id

    # Update mapid if it exists and has changed (resume in different terminal)
    if MAPID and existing_data and existing_data.get('mapid') != MAPID:
        updates['mapid'] = MAPID

    # Update transcript_path if provided
    if transcript_path:
        updates['transcript_path'] = transcript_path

    # Background instance metadata
    bg_env = os.environ.get('HCOM_BACKGROUND')
    if bg_env:
        updates['background'] = True
        updates['background_log_file'] = str(hcom_path(LOGS_DIR, bg_env))

    # Detect matched resume (same session_id, resumed Claude session)
    is_matched_resume = bool(existing_data)

    return instance_name, updates, is_matched_resume


def is_safe_hcom_command(command: str) -> bool:
    """Auto-approve hcom commands with safe chaining/redirects
    The security guarantee is: Only auto-approve if 100% certain it's safe hcom-only commands. If uncertain → ask
  for permission (one prompt).

    Security model:
    - Blocks command/variable injection: ` (everywhere), $VAR/${VAR}/$(cmd) (dangerous $), () (outside quotes only)
    - Allows harmless $: $.field (JSONPath), $5 (digits), $! (special chars)
    - ALL segments must be hcom commands (pure hcom-only)
    - Only /dev/null redirects allowed (no file writes)
    - Operators inside quotes treated as message text (safe)
    - Background execution (&) blocked
    """
    from ..shared import HCOM_COMMAND_PATTERN

    # Block backticks everywhere (command substitution)
    if '`' in command:
        return False

    # Block dangerous $ patterns (allow harmless ones like JSONPath '$.field')
    # Shell expands: $VAR, ${VAR}, $(cmd) but NOT $.field (. terminates expansion)
    # Check for actual variable expansion or command substitution
    if re.search(r'\$(?:[a-zA-Z_][a-zA-Z0-9_]*|\{|\()', command):
        return False

    cmd = command.strip()

    # Remove quoted strings to check for operators/redirects/parens outside quotes
    # This prevents "message with && inside" or "SQL with ()" from being treated as unsafe
    # DOTALL makes . match newlines so multiline quoted strings are properly stripped
    cmd_no_quotes = re.sub(r'''(['"])(?:(?=(\\?))\2.)*?\1''', '', cmd, flags=re.DOTALL)

    # Block command substitution parens (only outside quotes - inside quotes they're inert)
    if any(c in cmd_no_quotes for c in ['(', ')']):
        return False

    # Block output redirects to files (only allow /dev/null)
    if re.search(r'>\s*(?!&|/dev/null\b)\S+', cmd_no_quotes):
        return False

    # Block input redirects (check outside quotes only)
    # No legitimate use case for stdin in hcom commands
    if '<' in cmd_no_quotes:
        return False

    # Block background execution (& at end, not part of redirect)
    if re.search(r'&\s*$', cmd_no_quotes):
        return False

    # No operators outside quotes? Use simple validation
    if not any(c in cmd_no_quotes for c in ['&', '|', ';']):
        return bool(HCOM_COMMAND_PATTERN.match(cmd))

    # Protect redirects like 2>&1 from being split
    redirects_map = {}
    redirect_counter = [0]

    def save_redirect(match):
        placeholder = f'__REDIR_{redirect_counter[0]}__'
        redirects_map[placeholder] = match.group(0)
        redirect_counter[0] += 1
        return placeholder

    # Save all file descriptor redirects in cmd_no_quotes
    cmd_protected = re.sub(r'[012]?>&[012]|[012]?>/dev/null|&>/dev/null', save_redirect, cmd_no_quotes)

    # Split on operators (&&, ||, ;, |)
    segments = re.split(r'\s*(\&\&|\|\||;|\|)\s*', cmd_protected)

    if not segments:
        return False

    # Restore redirects in segments
    def restore_redirects(text):
        for placeholder, original in redirects_map.items():
            text = text.replace(placeholder, original)
        return text

    segments = [restore_redirects(s) for s in segments]

    # Validate ALL non-operator segments are hcom commands
    for segment in segments:
        segment = segment.strip()
        if not segment:
            continue

        # Is it an operator? Allow it
        if segment in ['&&', '||', ';', '|']:
            continue

        # Strip safe redirects for matching
        segment_clean = re.sub(r'\s+(?:2>&1|1>&2|[012]?>/dev/null|&>/dev/null)\s*$', '', segment).strip()

        # Must be hcom command
        if not HCOM_COMMAND_PATTERN.match(segment_clean):
            return False

    return True


