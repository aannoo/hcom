"""Main TUI orchestration"""
import os
import shlex
import shutil
import subprocess
import sys
import time
from pathlib import Path
from typing import List, Optional

# Import types
from .types import Mode, UIState
from .rendering import (
    ansi_len, ansi_ljust, truncate_ansi, get_terminal_size,
    get_message_pulse_colors, separator_line,
)
from .input import KeyboardInput

# Import from shared and api
from ..shared import (
    # ANSI codes
    RESET, BOLD, DIM,
    FG_GREEN, FG_CYAN, FG_WHITE, FG_BLACK, FG_GRAY, FG_YELLOW, FG_RED, FG_ORANGE,
    BG_ORANGE, BG_CHARCOAL, BG_YELLOW,
    CLEAR_SCREEN, CURSOR_HOME, HIDE_CURSOR, SHOW_CURSOR,
    # Config
    DEFAULT_CONFIG_HEADER,
    # Status configuration
    STATUS_ORDER, STATUS_BG_MAP,
    # Utilities
    format_timestamp, get_status_counts, parse_iso_timestamp,
    resolve_claude_args,
)
from ..api import (
    # Instance operations
    get_instance_status,
    # Path utilities
    hcom_path, ensure_hcom_directories,
    # Configuration
    HcomConfig, reload_config,
    ConfigSnapshot, load_config_snapshot, save_config,
    dict_to_hcom_config, HcomConfigError,
    # Commands
    cmd_launch, cmd_start, cmd_stop, cmd_reset, cmd_send,
)

# Import screens
from .manage import ManageScreen
from .launch import LaunchScreen

# Import config from parent package
from . import CONFIG_DEFAULTS

# TUI Layout Constants
MESSAGE_PREVIEW_LIMIT = 100  # Keep last N messages in message preview


from contextlib import contextmanager
from io import StringIO

@contextmanager
def _suppress_output():
    """Capture stdout/stderr to prevent CLI output from corrupting TUI display"""
    old_stdout, old_stderr = sys.stdout, sys.stderr
    sys.stdout, sys.stderr = StringIO(), StringIO()
    try:
        yield
    finally:
        sys.stdout, sys.stderr = old_stdout, old_stderr


class HcomTUI:
    """Main TUI application - orchestration only"""

    # Confirmation timeout constants
    CONFIRMATION_TIMEOUT = 10.0  # State cleared after this
    CONFIRMATION_FLASH_DURATION = 10.0  # Flash duration matches timeout

    def __init__(self, hcom_dir: Path):
        self.hcom_dir = hcom_dir
        self.mode = Mode.MANAGE
        self.state = UIState()  # All shared state in one place

        # Runtime orchestrator fields (not in UIState)
        self.last_frame = []
        self.last_status_update = 0.0
        self.last_config_check = 0.0
        self.first_render = True

        # Sync subprocess (for cross-device sync when no instances running)
        self.sync_proc = None

        # Screen instances (pass state + self)
        self.manage_screen = ManageScreen(self.state, self)
        self.launch_screen = LaunchScreen(self.state, self)

    def flash(self, msg: str, duration: float = 2.0, color: str = 'orange'):
        """Show temporary flash message

        Args:
            msg: Message text
            duration: Display time in seconds
            color: 'red', 'white', or 'orange' (default)
        """
        self.state.flash_message = msg
        self.state.flash_until = time.time() + duration
        self.state.flash_color = color
        self.state.frame_dirty = True

    def flash_error(self, msg: str, duration: float = 10.0):
        """Show error flash in red"""
        self.state.flash_message = msg
        self.state.flash_until = time.time() + duration
        self.state.flash_color = 'red'
        self.state.frame_dirty = True

    def parse_validation_errors(self, error_str: str):
        """Parse ValueError message from HcomConfig into field-specific errors"""
        self.state.validation_errors.clear()

        # Parse multi-line error format:
        # "Invalid config:\n  - timeout must be...\n  - terminal cannot..."
        for line in error_str.split('\n'):
            line = line.strip()
            if not line or line == 'Invalid config:':
                continue

            # Remove leading "- " from error lines
            if line.startswith('- '):
                line = line[2:]

            # Match error to field based on keywords
            # For fields with multiple possible errors, only store first error seen
            line_lower = line.lower()
            if 'timeout must be' in line_lower and 'subagent' not in line_lower:
                if 'HCOM_TIMEOUT' not in self.state.validation_errors:
                    self.state.validation_errors['HCOM_TIMEOUT'] = line
            elif 'subagent_timeout' in line_lower or 'subagent timeout' in line_lower:
                if 'HCOM_SUBAGENT_TIMEOUT' not in self.state.validation_errors:
                    self.state.validation_errors['HCOM_SUBAGENT_TIMEOUT'] = line
            elif 'terminal' in line_lower:
                if 'HCOM_TERMINAL' not in self.state.validation_errors:
                    self.state.validation_errors['HCOM_TERMINAL'] = line
            elif 'tag' in line_lower:
                if 'HCOM_TAG' not in self.state.validation_errors:
                    self.state.validation_errors['HCOM_TAG'] = line
            elif 'claude_args' in line_lower:
                if 'HCOM_CLAUDE_ARGS' not in self.state.validation_errors:
                    self.state.validation_errors['HCOM_CLAUDE_ARGS'] = line
            elif 'hints' in line_lower:
                if 'HCOM_HINTS' not in self.state.validation_errors:
                    self.state.validation_errors['HCOM_HINTS'] = line

    def clear_all_pending_confirmations(self):
        """Clear all pending confirmation states and flash if any were active"""
        had_pending = self.state.pending_toggle or self.state.pending_stop_all or self.state.pending_reset

        self.state.pending_toggle = None
        self.state.pending_stop_all = False
        self.state.pending_reset = False

        if had_pending:
            self.state.flash_message = None

    def clear_pending_confirmations_except(self, keep: str):
        """Clear all pending confirmations except the specified one ('toggle', 'stop_all', 'reset')"""
        had_pending = False

        if keep != 'toggle' and self.state.pending_toggle:
            self.state.pending_toggle = None
            had_pending = True
        if keep != 'stop_all' and self.state.pending_stop_all:
            self.state.pending_stop_all = False
            had_pending = True
        if keep != 'reset' and self.state.pending_reset:
            self.state.pending_reset = False
            had_pending = True

        if had_pending:
            self.state.flash_message = None

    def stop_all_instances(self):
        """Stop all enabled instances"""
        try:
            # Count enabled instances before stopping
            enabled_before = sum(1 for info in self.state.instances.values() if info['data'].get('enabled', False))

            # Suppress CLI output to prevent TUI corruption
            with _suppress_output():
                result = cmd_stop(['all'])
            if result == 0:
                self.load_status()
                # Count how many were actually stopped
                enabled_after = sum(1 for info in self.state.instances.values() if info['data'].get('enabled', False))
                stopped_count = enabled_before - enabled_after

                if stopped_count > 0:
                    self.flash(f"Stopped {stopped_count} instance{'s' if stopped_count != 1 else ''}")
                else:
                    self.flash("No instances to stop")
            else:
                self.flash_error("Failed to stop instances")
        except Exception as e:
            self.flash_error(f"Error: {str(e)}")

    def reset_events(self):
        """Reset events (archive and clear database)"""
        try:
            # Close stale connection before reset (clear() deletes DB file)
            from ..core.db import close_db
            close_db()

            # Suppress CLI output to prevent TUI corruption
            with _suppress_output():
                result = cmd_reset([])
            if result == 0:
                # Clear message state
                self.state.messages = []
                self.state.last_event_id = 0
                self.state.device_sync_times = {}  # Clear cached sync times
                # Reload to clear instance list from display
                self.load_status()
                archive_path = f"{Path.home()}/.hcom/archive/"
                self.flash(f"Logs and instance list archived to {archive_path}", duration=10.0)
            else:
                self.flash_error("Failed to reset events")
        except Exception as e:
            self.flash_error(f"Error: {str(e)}")

    def run(self) -> int:
        """Main event loop"""
        # Initialize
        ensure_hcom_directories()

        # Load saved states (config.env first, then launch state reads from it)
        self.load_config_from_file()
        self.load_launch_state()

        # Enter alternate screen
        sys.stdout.write('\033[?1049h')
        sys.stdout.flush()

        try:
            with KeyboardInput() as kbd:
                while True:
                    # Only update/render if no pending input (paste optimization)
                    if not kbd.has_input():
                        self.update()
                        self.render()
                        time.sleep(0.01)  # Only sleep when idle

                    key = kbd.get_key()
                    if not key:
                        time.sleep(0.01)  # Also sleep when no key available
                        continue

                    if key == 'CTRL_D':
                        # Save state before exit
                        self.save_launch_state()
                        break
                    elif key == 'TAB':
                        # Save state when switching modes
                        if self.mode == Mode.LAUNCH:
                            self.save_launch_state()
                        self.handle_tab()
                        self.state.frame_dirty = True
                    else:
                        self.handle_key(key)
                        self.state.frame_dirty = True

            return 0
        except KeyboardInterrupt:
            # Ctrl+C - clean exit
            self.save_launch_state()
            return 0
        except Exception as e:
            # Restore terminal BEFORE writing error (so it's visible)
            sys.stdout.write('\033[?1049l')  # Exit alternate screen
            sys.stdout.write('\033[?25h')     # Show cursor
            sys.stdout.flush()
            # Now write error with traceback
            import traceback
            sys.stderr.write(f"\nError: {e}\n")
            traceback.print_exc()
            return 1
        finally:
            # Cleanup sync subprocess
            if self.sync_proc:
                try:
                    self.sync_proc.terminate()
                except Exception:
                    pass
                self.sync_proc = None

            # Ensure terminal restored (idempotent)
            sys.stdout.write('\033[?1049l')
            sys.stdout.write('\033[?25h')
            sys.stdout.flush()

    def load_status(self):
        """Load instance status from DB (streamed, not all at once)"""
        import sqlite3
        from ..core.db import iter_instances, close_db

        try:
            # Stream instances from DB
            instances = {}
            for data in iter_instances():
                instances[data['name']] = data
        except sqlite3.OperationalError:
            # DB was deleted/reset by another process - reconnect
            close_db()
            instances = {}
            for data in iter_instances():
                instances[data['name']] = data

        # Build instance info dict (replace old instances, don't just add)
        new_instances = {}
        for name, data in instances.items():
            enabled, status_type, age_text, description, age_seconds = get_instance_status(data)

            new_instances[name] = {
                'enabled': enabled,
                'status': status_type,
                'age_text': age_text,
                'description': description,
                'age_seconds': age_seconds,
                'data': data,
            }

        self.state.instances = new_instances
        # Status counts only for enabled instances (header shows active participants)
        enabled_instances = {k: v for k, v in self.state.instances.items() if v.get('enabled', False)}
        self.state.status_counts = get_status_counts(enabled_instances)

        # Load launch status (for banner) - aggregates all pending batches
        try:
            from ..core.db import get_launch_status, parse_iso_timestamp, LAUNCH_TIMEOUT_SECONDS
            from datetime import datetime, timezone
            import time as time_module

            # Clear failed banner after 5s timeout
            if self.state.launch_batch_failed and time_module.time() >= self.state.launch_batch_failed_until:
                self.state.launch_batch = None
                self.state.launch_batch_failed = False

            batch = get_launch_status()  # No launcher filter - show all pending
            # Only show if not complete AND recent (timeout for failed launches)
            if batch and batch['ready'] < batch['expected']:
                ts = parse_iso_timestamp(batch.get('timestamp', ''))
                cutoff = datetime.now(timezone.utc).timestamp() - LAUNCH_TIMEOUT_SECONDS
                if ts and ts.timestamp() > cutoff:
                    self.state.launch_batch = batch
                    self.state.launch_batch_failed = False
                else:
                    # Batch timed out - show red banner for 5s
                    if self.state.launch_batch and not self.state.launch_batch_failed:
                        self.state.launch_batch_failed = True
                        self.state.launch_batch_failed_until = time_module.time() + 5.0
                    self.state.launch_batch = batch  # Keep batch for red banner
            else:
                self.state.launch_batch = None
                self.state.launch_batch_failed = False
        except Exception:
            self.state.launch_batch = None

        # Load device sync times for remote instance pulse coloring
        try:
            from ..core.db import get_db, kv_get
            conn = get_db()
            # Get unique remote device IDs
            rows = conn.execute(
                "SELECT DISTINCT origin_device_id FROM instances WHERE origin_device_id IS NOT NULL AND origin_device_id != ''"
            ).fetchall()
            device_times = {}
            for row in rows:
                device_id = row['origin_device_id']
                ts = kv_get(f'relay_sync_time_{device_id}')
                if ts:
                    device_times[device_id] = float(ts)
            self.state.device_sync_times = device_times
        except Exception:
            pass  # Keep existing sync times if query fails

        # Load relay status for status bar indicator
        try:
            from ..relay import get_relay_status
            status = get_relay_status()
            self.state.relay_configured = status['configured']
            self.state.relay_enabled = status['enabled']
            self.state.relay_status = status['status']
            self.state.relay_error = status['error']
        except Exception:
            pass

    def save_launch_state(self):
        """Save launch form values to config.env via claude args parser"""
        # Phase 3: Save Claude args to HCOM_CLAUDE_ARGS in config.env
        try:
            # Load current spec
            claude_args_str = self.state.config_edit.get('HCOM_CLAUDE_ARGS', '')
            spec = resolve_claude_args(None, claude_args_str if claude_args_str else None)

            # Update spec with background and prompt
            spec = spec.update(
                background=self.state.launch_background,
                prompt=self.state.launch_prompt,  # Always pass value (empty string deletes)
            )

            # Build tokens manually to support both system prompt types
            tokens = list(spec.clean_tokens)

            # Add system prompts if present
            if self.state.launch_system_prompt:
                tokens.extend(['--system-prompt', self.state.launch_system_prompt])
            if self.state.launch_append_system_prompt:
                tokens.extend(['--append-system-prompt', self.state.launch_append_system_prompt])

            # Re-parse to get proper spec
            spec = resolve_claude_args(tokens, None)

            # Persist to in-memory edits
            self.state.config_edit['HCOM_CLAUDE_ARGS'] = spec.to_env_string()

            # Write config.env
            # Note: HCOM_TAG is already saved directly when edited in UI
            self.save_config_to_file()
        except Exception as e:
            # Don't crash on save failure, but log to stderr
            sys.stderr.write(f"Warning: Failed to save launch state: {e}\n")

    def load_launch_state(self):
        """Load launch form values from config.env via claude args parser"""
        # Phase 3: Load Claude args from HCOM_CLAUDE_ARGS in config.env
        try:
            claude_args_str = self.state.config_edit.get('HCOM_CLAUDE_ARGS', '')
            spec = resolve_claude_args(None, claude_args_str if claude_args_str else None)

            # Check for parse errors and surface them
            if spec.errors:
                self.flash_error(f"Parse error: {spec.errors[0]}")

            # Extract Claude-related fields from spec
            self.state.launch_background = spec.is_background
            self.state.launch_prompt = spec.positional_tokens[0] if spec.positional_tokens else ""

            # Extract both system prompt types
            self.state.launch_system_prompt = spec.user_system or ""
            self.state.launch_append_system_prompt = spec.user_append or ""

            # Clamp cursors to valid range (preserve position if within bounds)
            self.state.launch_prompt_cursor = min(self.state.launch_prompt_cursor, len(self.state.launch_prompt))
            self.state.launch_system_prompt_cursor = min(self.state.launch_system_prompt_cursor, len(self.state.launch_system_prompt))
            self.state.launch_append_system_prompt_cursor = min(self.state.launch_append_system_prompt_cursor, len(self.state.launch_append_system_prompt))
        except Exception as e:
            # Failed to parse - use defaults and log warning
            sys.stderr.write(f"Warning: Failed to load launch state (using defaults): {e}\n")

    def load_config_from_file(self, *, raise_on_error: bool = False):
        """Load all vars from ~/.hcom/config.env into editable dict"""
        config_path = Path.home() / '.hcom' / 'config.env'
        try:
            snapshot = load_config_snapshot()
            combined: dict[str, str] = {}
            combined.update(snapshot.values)
            combined.update(snapshot.extras)
            self.state.config_edit = combined
            self.state.validation_errors.clear()
            # Track mtime for external change detection
            try:
                self.state.config_mtime = config_path.stat().st_mtime
            except FileNotFoundError:
                self.state.config_mtime = 0.0
        except Exception as e:
            if raise_on_error:
                raise
            sys.stderr.write(f"Warning: Failed to load config.env (using defaults): {e}\n")
            self.state.config_edit = dict(CONFIG_DEFAULTS)
            for line in DEFAULT_CONFIG_HEADER:
                stripped = line.strip()
                if stripped and not stripped.startswith('#') and '=' in line:
                    key, _, value = line.partition('=')
                    key = key.strip()
                    raw = value.strip()
                    if (raw.startswith('"') and raw.endswith('"')) or (raw.startswith("'") and raw.endswith("'")):
                        raw = raw[1:-1]
                    self.state.config_edit.setdefault(key, raw)
            self.state.config_mtime = 0.0

    def save_config_to_file(self):
        """Write current config edits back to ~/.hcom/config.env using canonical writer."""
        known_values = {key: self.state.config_edit.get(key, '') for key in CONFIG_DEFAULTS.keys()}
        extras = {
            key: value
            for key, value in self.state.config_edit.items()
            if key not in CONFIG_DEFAULTS
        }

        field_map = {
            'timeout': 'HCOM_TIMEOUT',
            'subagent_timeout': 'HCOM_SUBAGENT_TIMEOUT',
            'terminal': 'HCOM_TERMINAL',
            'tag': 'HCOM_TAG',
            'claude_args': 'HCOM_CLAUDE_ARGS',
            'hints': 'HCOM_HINTS',
        }

        try:
            core = dict_to_hcom_config(known_values)
        except HcomConfigError as exc:
            self.state.validation_errors.clear()
            for field, message in exc.errors.items():
                env_key = field_map.get(field, field.upper())
                self.state.validation_errors[env_key] = message
            first_error = next(iter(self.state.validation_errors.values()), "Invalid config")
            self.flash_error(first_error)
            return
        except Exception as exc:
            self.flash_error(f"Validation error: {exc}")
            return

        try:
            save_config(core, extras)
            self.state.validation_errors.clear()
            self.state.flash_message = None
            # Reload snapshot to pick up canonical formatting
            self.load_config_from_file()
            self.load_launch_state()
            # Refresh runtime config cache (for relay, etc.)
            reload_config()
            # Update relay status in UI state
            self.load_status()
        except Exception as exc:
            self.flash_error(f"Save failed: {exc}")

    def check_external_config_changes(self):
        """Reload config.env if changed on disk, preserving active edits."""
        config_path = Path.home() / '.hcom' / 'config.env'
        try:
            mtime = config_path.stat().st_mtime
        except FileNotFoundError:
            return

        if mtime <= self.state.config_mtime:
            return  # No change

        # Save what's currently being edited
        active_field = self.launch_screen.get_current_field_info()

        # Backup current edits
        old_edit = dict(self.state.config_edit)

        # Reload from disk
        try:
            self.load_config_from_file()
            self.load_launch_state()
            reload_config()  # Refresh runtime cache
        except Exception as exc:
            self.flash_error(f"Failed to reload config.env: {exc}")
            return

        # Update mtime
        try:
            self.state.config_mtime = config_path.stat().st_mtime
        except FileNotFoundError:
            self.state.config_mtime = 0.0

        self.state.frame_dirty = True

        # Restore in-progress edit if field changed externally
        if active_field and active_field[0]:
            key, value, cursor = active_field
            # Check if the field we're editing changed externally
            if key in old_edit and old_edit.get(key) != self.state.config_edit.get(key):
                # External change to field you're editing - keep your version
                self.state.config_edit[key] = value
                if key in self.state.config_field_cursors:
                    self.state.config_field_cursors[key] = cursor
                self.flash(f"Kept in-progress {key} edit (external change ignored)")

    def resolve_editor_command(self) -> tuple[list[str] | None, str | None]:
        """Resolve preferred editor command and display label for config edits."""
        config_path = Path.home() / '.hcom' / 'config.env'
        editor = os.environ.get('VISUAL') or os.environ.get('EDITOR')
        pretty_names = {
            'code': 'VS Code',
            'code-insiders': 'VS Code Insiders',
            'hx': 'Helix',
            'helix': 'Helix',
            'nvim': 'Neovim',
            'vim': 'Vim',
            'nano': 'nano',
        }

        if editor:
            try:
                parts = shlex.split(editor)
            except ValueError:
                parts = []
            if parts:
                command = parts[0]
                base_name = Path(command).name or command
                normalized = base_name.lower()
                if normalized.endswith('.exe'):
                    normalized = normalized[:-4]
                label = pretty_names.get(normalized, base_name)
                return parts + [str(config_path)], label

        if code_bin := shutil.which('code'):
            return [code_bin, str(config_path)], 'VS Code'
        if nano_bin := shutil.which('nano'):
            return [nano_bin, str(config_path)], 'nano'
        if vim_bin := shutil.which('vim'):
            return [vim_bin, str(config_path)], 'vim'
        return None, None

    def open_config_in_editor(self):
        """Open config.env in the resolved editor."""
        cmd, label = self.resolve_editor_command()
        if not cmd:
            self.flash_error("No external editor found")
            return

        # Ensure latest in-memory edits are persisted before handing off
        self.save_config_to_file()

        try:
            subprocess.Popen(cmd)
            self.flash(f"Opening config.env in {label or 'VS Code'}...")
        except Exception as exc:
            self.flash_error(f"Failed to launch {label or 'editor'}: {exc}")


    def update(self):
        """Update state (status, messages)"""
        now = time.time()

        # Clear expired flash messages
        if self.state.flash_message and now >= self.state.flash_until:
            self.state.flash_message = None
            self.state.frame_dirty = True

        # Clear expired send state
        if self.state.send_state == 'sent' and now >= self.state.send_state_until:
            self.state.send_state = None
            self.state.frame_dirty = True

        # Update interval: faster during active pulse animation, slower when idle
        pulse_active = (self.state.last_message_time > 0 and
                       now - self.state.last_message_time < 5.0)
        update_interval = 0.1 if pulse_active else 0.5

        if now - self.last_status_update >= update_interval:
            # Check if sync subprocess finished (non-blocking)
            if self.sync_proc and self.sync_proc.poll() is not None:
                if self.sync_proc.returncode == 0:
                    self.state.frame_dirty = True  # New data arrived
                self.sync_proc = None

            # Start sync subprocess if no enabled instances and not already running
            enabled_count = sum(1 for i in self.state.instances.values() if i.get('enabled'))
            if enabled_count == 0 and self.sync_proc is None:
                try:
                    from ..relay import is_relay_enabled
                    if is_relay_enabled():  # Only if relay configured AND enabled
                        self.sync_proc = subprocess.Popen(
                            [sys.executable, '-m', 'hcom', 'relay', 'poll', '25'],
                            stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL,
                        )
                except Exception:
                    pass  # Ignore sync errors

            self.load_status()
            self.last_status_update = now
            self.state.frame_dirty = True

        # Clear pending toggle after timeout
        if self.state.pending_toggle and (now - self.state.pending_toggle_time) > self.CONFIRMATION_TIMEOUT:
            self.state.pending_toggle = None
            self.state.show_instance_detail = None
            self.state.frame_dirty = True

        # Clear completed toggle display after 2s (match flash default)
        if self.state.completed_toggle and (now - self.state.completed_toggle_time) >= 2.0:
            self.state.completed_toggle = None
            self.state.frame_dirty = True

        # Clear pending stop all after timeout
        if self.state.pending_stop_all and (now - self.state.pending_stop_all_time) > self.CONFIRMATION_TIMEOUT:
            self.state.pending_stop_all = False
            self.state.frame_dirty = True

        # Clear pending reset after timeout
        if self.state.pending_reset and (now - self.state.pending_reset_time) > self.CONFIRMATION_TIMEOUT:
            self.state.pending_reset = False
            self.state.frame_dirty = True

        # Periodic config reload check (detects external changes from CLI, editor, etc.)
        if (now - self.last_config_check) >= 0.5:
            self.last_config_check = now
            self.check_external_config_changes()

        # Load messages for MANAGE screen preview (with event ID caching)
        if self.mode == Mode.MANAGE:
            from ..core.db import get_last_event_id, get_events_since
            import json

            try:
                current_max_id = get_last_event_id()
                # Detect external reset: max ID dropped means DB was cleared
                if current_max_id < self.state.last_event_id:
                    self.state.messages = []
                    self.state.last_event_id = 0
                    self.state.frame_dirty = True
                if current_max_id != self.state.last_event_id:
                    events = get_events_since(self.state.last_event_id, event_type='message')
                    new_messages = []
                    for e in events:
                        event_data = e['data']  # Already a dict from db.py
                        # Include delivered_to and event_id for read receipt tracking
                        delivered_to = event_data.get('delivered_to', [])
                        new_messages.append((
                            e['timestamp'],
                            event_data.get('from', ''),
                            event_data.get('text', ''),
                            delivered_to,
                            e['id']  # event_id for read receipt lookup
                        ))

                    # Append new messages and keep last N
                    all_messages = list(self.state.messages) + new_messages
                    self.state.messages = all_messages[-MESSAGE_PREVIEW_LIMIT:] if len(all_messages) > MESSAGE_PREVIEW_LIMIT else all_messages

                    # Update last message time for EVENTS tab pulse
                    if all_messages:
                        last_msg_timestamp = all_messages[-1][0]
                        dt = parse_iso_timestamp(last_msg_timestamp) if 'T' in last_msg_timestamp else None
                        self.state.last_message_time = dt.timestamp() if dt else 0.0
                    else:
                        self.state.last_message_time = 0.0

                    self.state.last_event_id = current_max_id
                    self.state.frame_dirty = True
            except Exception as e:
                # DB query failed - flash error and keep existing messages
                self.flash_error(f"Message load failed: {e}", duration=5.0)

    def build_status_bar(self, highlight_tab: str | None = None) -> str:
        """Build status bar with tabs - shared by TUI header and native events view
        Args:
            highlight_tab: Which tab to highlight ("MANAGE", "LAUNCH", or "EVENTS")
                          If None, uses self.mode
        """
        # Determine which tab to highlight
        if highlight_tab is None:
            highlight_tab = self.mode.value.upper()

        # Calculate message pulse colors for EVENTS tab
        if self.state.last_message_time > 0:
            seconds_since_msg = time.time() - self.state.last_message_time
        else:
            seconds_since_msg = 9999.0  # No messages yet - use quiet state
        log_bg_color, log_fg_color = get_message_pulse_colors(seconds_since_msg)

        # Build status display (colored blocks for unselected, orange for selected)
        is_manage_selected = (highlight_tab == "MANAGE")
        status_parts = []

        # Use shared status configuration (background colors for statusline blocks)
        for status_type in STATUS_ORDER:
            count = self.state.status_counts.get(status_type, 0)
            if count > 0:
                color, symbol = STATUS_BG_MAP[status_type]
                if is_manage_selected:
                    # Selected: orange bg + black text (v1 style)
                    part = f"{FG_BLACK}{BOLD}{BG_ORANGE} {count} {symbol} {RESET}"
                else:
                    # Unselected: colored blocks (hcom watch style)
                    text_color = FG_BLACK if color == BG_YELLOW else FG_WHITE
                    part = f"{text_color}{BOLD}{color} {count} {symbol} {RESET}"
                status_parts.append(part)

        # No instances - show MANAGE text instead of 0
        if status_parts:
            status_display = "".join(status_parts)
        elif is_manage_selected:
            status_display = f"{FG_BLACK}{BOLD}{BG_ORANGE} MANAGE {RESET}"
        else:
            status_display = f"{BG_CHARCOAL}{FG_WHITE} MANAGE {RESET}"

        # Build tabs: MANAGE, LAUNCH, and EVENTS (EVENTS only shown in native view)
        tab_names = ["MANAGE", "LAUNCH", "EVENTS"]
        tabs = []

        for tab_name in tab_names:
            # MANAGE tab shows status counts instead of text
            if tab_name == "MANAGE":
                label = status_display
            else:
                label = tab_name

            # Highlight current tab (non-MANAGE tabs get orange bg)
            if tab_name == highlight_tab and tab_name != "MANAGE":
                # Selected tab: always orange bg + black fg (EVENTS and LAUNCH same)
                tabs.append(f"{BG_ORANGE}{FG_BLACK}{BOLD} {label} {RESET}")
            elif tab_name == "MANAGE":
                # MANAGE tab is just status blocks (already has color/bg)
                tabs.append(f" {label}")
            elif tab_name == "EVENTS":
                # EVENTS tab when not selected: use pulse colors (white→charcoal fade)
                tabs.append(f"{log_bg_color}{log_fg_color} {label} {RESET}")
            else:
                # LAUNCH when not selected: charcoal bg (milder than black)
                tabs.append(f"{BG_CHARCOAL}{FG_WHITE} {label} {RESET}")

        tab_display = " ".join(tabs)

        # Relay indicator - only show if configured AND enabled
        relay_indicator = ""
        if self.state.relay_configured and self.state.relay_enabled:
            if self.state.relay_status == 'error':
                icon = f"{FG_RED}⇄{RESET}"
                err = self.state.relay_error
                relay_indicator = f" {icon} {err}" if err else f" {icon}"
            elif self.state.relay_status == 'ok':
                icon = f"{FG_GREEN}⇄{RESET}"
                relay_indicator = f" {icon}"
            else:
                # Never connected yet
                icon = f"{FG_GRAY}⇄{RESET}"
                relay_indicator = f" {icon}"

        return f"{BOLD}hcom{RESET} {tab_display}{relay_indicator}"

    def build_flash(self) -> Optional[str]:
        """Build flash notification if active"""
        if self.state.flash_message and time.time() < self.state.flash_until:
            color_map = {
                'red': FG_RED,
                'white': FG_WHITE,
                'orange': FG_ORANGE
            }
            color_code = color_map.get(self.state.flash_color, FG_ORANGE)
            cols, _ = get_terminal_size()
            # Reserve space for "• " prefix and separator/padding
            max_msg_width = cols - 10
            msg = truncate_ansi(self.state.flash_message, max_msg_width) if len(self.state.flash_message) > max_msg_width else self.state.flash_message
            return f"{BOLD}{color_code}• {msg}{RESET}"
        return None

    def render(self):
        """Render current screen"""
        # Skip rebuild if nothing changed
        if not self.state.frame_dirty:
            return

        cols, rows = get_terminal_size()
        # Adapt to any terminal size
        rows = max(10, rows)

        frame = []

        # Header (compact - no separator)
        header = self.build_status_bar()
        frame.append(ansi_ljust(header, cols))

        # Flash row with separator line
        flash = self.build_flash()
        if flash:
            # Flash message on left, separator line fills rest of row
            flash_len = ansi_len(flash)
            remaining = cols - flash_len - 1  # -1 for space
            sep = separator_line(remaining) if remaining > 0 else ""
            frame.append(f"{flash} {sep}")
        else:
            # Just separator line when no flash message
            frame.append(separator_line(cols))

        # Welcome message on first render
        if self.first_render:
            self.flash("Welcome! Tab to switch screens")
            self.first_render = False

        # Body (subtract 3: header, flash, footer)
        body_rows = rows - 3

        if self.mode == Mode.MANAGE:
            manage_lines = self.manage_screen.build(body_rows, cols)
            for line in manage_lines:
                frame.append(ansi_ljust(line, cols))
        elif self.mode == Mode.LAUNCH:
            form_lines = self.launch_screen.build(body_rows, cols)
            for line in form_lines:
                frame.append(ansi_ljust(line, cols))

        # Footer - compact help text
        if self.mode == Mode.MANAGE:
            # Contextual footer based on state
            if self.state.message_buffer.strip():
                footer = f"{FG_GRAY}tab: switch  @: mention  enter: send  esc: clear{RESET}"
            elif self.state.pending_stop_all:
                footer = f"{FG_GRAY}ctrl+a: confirm stop all  esc: cancel{RESET}"
            elif self.state.pending_reset:
                footer = f"{FG_GRAY}ctrl+r: confirm clear  esc: cancel{RESET}"
            elif self.state.pending_toggle:
                footer = f"{FG_GRAY}enter: confirm  esc: cancel{RESET}"
            else:
                footer = f"{FG_GRAY}tab: switch  @: mention  enter: toggle  ctrl+a: stop all  ctrl+r: clear{RESET}"
        elif self.mode == Mode.LAUNCH:
            footer = self.launch_screen.get_footer()
        frame.append(truncate_ansi(footer, cols))

        # Repaint if changed
        if frame != self.last_frame:
            sys.stdout.write(CLEAR_SCREEN + CURSOR_HOME)
            for i, line in enumerate(frame):
                sys.stdout.write(line)
                if i < len(frame) - 1:
                    sys.stdout.write('\n')
            sys.stdout.flush()
            self.last_frame = frame

        # Frame rebuilt - clear dirty flag
        self.state.frame_dirty = False

    def handle_tab(self):
        """Cycle between Manage, Launch, and native Log view"""
        if self.mode == Mode.MANAGE:
            self.mode = Mode.LAUNCH
            self.flash("Launch Instances")
        elif self.mode == Mode.LAUNCH:
            # Go directly to native events view instead of alternate mode
            self.flash(f"Event History {RESET}{FG_ORANGE}- type to filter")
            self.show_events_native()
            # After returning from native view, go to MANAGE
            self.mode = Mode.MANAGE
            self.flash("Manage Instances")

    def format_multiline_event(self, display_time: str, sender: str, message: str, type_prefix: str = '', sender_padded: str = '') -> List[str]:
        """Format event with multiline support (indented continuation lines)
        Format: time name: [type] content
        """
        display_sender = sender_padded if sender_padded else sender

        if '\n' not in message:
            return [f"{DIM}{FG_GRAY}{display_time}{RESET} {BOLD}{FG_ORANGE}{display_sender}{RESET}: {type_prefix}{message}"]

        lines = message.split('\n')
        result = [f"{DIM}{FG_GRAY}{display_time}{RESET} {BOLD}{FG_ORANGE}{display_sender}{RESET}: {type_prefix}{lines[0]}"]
        # Calculate indent: time + sender + ": [message] "
        # type_prefix is "[message] " (10 chars)
        type_prefix_len = len(type_prefix)
        indent = ' ' * (len(display_time) + len(display_sender) + 2 + type_prefix_len)
        result.extend(indent + line for line in lines[1:])
        return result

    def render_status_with_separator(self, highlight_tab: str = "EVENTS"):
        """Render separator line and status bar (extracted helper)"""
        cols, _ = get_terminal_size()

        # Separator or flash line
        flash = self.build_flash()
        if flash:
            flash_len = ansi_len(flash)
            remaining = cols - flash_len - 1
            sep = separator_line(remaining) if remaining > 0 else ""
            print(f"{flash} {sep}")
        else:
            print(separator_line(cols))

        # Status line
        safe_width = cols - 2
        status = truncate_ansi(self.build_status_bar(highlight_tab=highlight_tab), safe_width)
        sys.stdout.write(status)
        sys.stdout.flush()

    def sanitize_filter_input(self, text: str) -> str:
        """Remove dangerous chars, limit length for filter input"""
        # Strip control chars except printable
        cleaned = ''.join(c for c in text if c.isprintable() or c in ' \t')
        # Truncate to prevent paste bombs
        return cleaned[:200]

    def matches_filter(self, event: dict, query: str) -> bool:
        """Check if event matches query (multi-word AND). May raise KeyError/TypeError."""
        if not query or not query.strip():
            return True  # Empty query = show all

        # Split query into words (AND logic)
        words = [w.casefold() for w in query.split()]

        # Build searchable string from all event fields
        data = event.get('data', '')
        if isinstance(data, dict):
            # Extract values from dict for better searchability
            data_str = ' '.join(str(v) for v in data.values())
        elif isinstance(data, str):
            data_str = data
        else:
            data_str = str(data)

        searchable = (
            event.get('type', '') + ' ' +
            event.get('instance', '') + ' ' +
            data_str
        ).casefold()

        # All words must match (AND)
        return all(word in searchable for word in words)

    def matches_filter_safe(self, event: dict, query: str) -> bool:
        """Match event against query with error boundary"""
        try:
            return self.matches_filter(event, query)
        except (KeyError, TypeError, AttributeError, UnicodeDecodeError) as e:
            # Malformed event or encoding issue - treat as non-match
            import sys
            print(f"DEBUG: Event {event.get('id', '?')} match failed: {e}", file=sys.stderr)
            return False

    def render_event(self, event: dict):
        """Render event by type with defensive defaults
        Format: time name: [type] content
        """
        event_type = event.get('type', 'unknown')
        timestamp = event.get('timestamp', '')
        instance = event.get('instance', '?')
        data = event.get('data', {})

        # Always show type label in brackets
        type_labels = {
            'message': 'message',
            'status': 'status',
            'life': 'life'
        }
        type_label = type_labels.get(event_type, event_type)

        if event_type == 'message':
            # Format: time name [message]\ncontent
            sender = data.get('from', '?')
            message = data.get('text', '')
            display_time = format_timestamp(timestamp)
            print(f"{DIM}{FG_GRAY}{display_time}{RESET} {BOLD}{FG_ORANGE}{sender}{RESET} {FG_GRAY}[{type_label}]{RESET}")
            print(message)
            print()  # Empty line between events

        elif event_type == 'status':
            # Format: time name: [status] status, context
            status = data.get('status', '?')
            context = data.get('context', '')
            # Add comma before context if present
            ctx = f", {context}" if context else ""
            print(f"{DIM}{FG_GRAY}{format_timestamp(timestamp)}{RESET} "
                  f"{BOLD}{FG_CYAN}{instance}{RESET}: {FG_GRAY}[{type_label}]{RESET} {status}{ctx}")
            print()  # Empty line between events

        elif event_type == 'life':
            # Format: time name: [life] action
            action = data.get('action', '?')
            print(f"{DIM}{FG_GRAY}{format_timestamp(timestamp)}{RESET} "
                  f"{BOLD}{FG_YELLOW}{instance}{RESET}: {FG_GRAY}[{type_label}]{RESET} {action}")
            print()  # Empty line between events

        else:
            # Unknown type - generic fallback
            print(f"{DIM}{FG_GRAY}{format_timestamp(timestamp)}{RESET} "
                  f"{BOLD}{instance}{RESET}: {FG_GRAY}[{event_type}]{RESET} {data}")
            print()  # Empty line between events

    def render_event_safe(self, event: dict):
        """Render event with fallback for malformed data"""
        try:
            self.render_event(event)
        except Exception as e:
            # Complete rendering failure - show minimal fallback
            event_id = event.get('id', '?')
            print(f"{FG_GRAY}[malformed event {event_id}]{RESET}")
            print()
            import sys
            print(f"DEBUG: Render failed for event {event_id}: {e}", file=sys.stderr)

    def _render_events_bottom(self, cols: int, matched: int, total: int, use_write: bool = False):
        """Render bottom rows for events view (filter/separator + status bar)"""
        filter_active = bool(self.state.event_filter.strip())

        # First row: filter line or separator/flash
        if filter_active:
            filter_prefix = "Filter: "
            available = cols - len(filter_prefix) - 20
            filter_text = self.state.event_filter[:available] if len(self.state.event_filter) > available else self.state.event_filter
            cursor_display = "_" if self.state.event_filter_cursor == len(self.state.event_filter) else ""
            filter_display = filter_text[:self.state.event_filter_cursor] + cursor_display + filter_text[self.state.event_filter_cursor:]
            count_str = f" [{matched}/{total}]"
            first_line = truncate_ansi(f"{filter_prefix}{filter_display}{count_str}", cols)
        else:
            flash = self.build_flash()
            if flash:
                flash_len = ansi_len(flash)
                remaining = cols - flash_len - 1
                sep = separator_line(remaining) if remaining > 0 else ""
                first_line = f"{flash} {sep}"
            else:
                first_line = separator_line(cols)

        # Output first line
        if use_write:
            sys.stdout.write(first_line + '\n')
        else:
            print(first_line)

        # Status bar with enter indicator and current type
        type_suffix = f" {DIM}[ ↵ {self.state.event_type_filter} ]{RESET}"
        status = self.build_status_bar(highlight_tab="EVENTS") + type_suffix
        sys.stdout.write(truncate_ansi(status, cols))
        sys.stdout.flush()

    def show_events_native(self):
        """Exit TUI, show streaming events in native buffer with filtering support"""
        # Clear filter on entry (fresh start each time)
        self.state.event_filter = ""
        self.state.event_filter_cursor = 0

        # Exit alt screen
        sys.stdout.write('\033[?1049l' + SHOW_CURSOR)
        sys.stdout.flush()

        def redraw_all():
            """Redraw entire event list with filtering (on entry or resize)"""
            from ..core.db import get_events_since, get_last_event_id

            # Clear screen
            sys.stdout.write('\033[2J\033[H')
            sys.stdout.flush()

            # Initialize counts
            matched_count = 0
            total_count = 0

            # Re-render all messages
            try:
                # Get all messages matching current filter
                # Note: We use a fresh query here to ensure we have everything
                # even if the incremental updates missed something
                event_type = None if self.state.event_type_filter == "all" else self.state.event_type_filter
                events = get_events_since(0, event_type=event_type)
                total_count = len(events)
                has_events = bool(events)

                if events:
                    # Filter and render all matching events
                    for event in events:
                        if self.matches_filter_safe(event, self.state.event_filter):
                            self.render_event_safe(event)
                            matched_count += 1

                    # Show message if no matches when filtering
                    if matched_count == 0 and self.state.event_filter.strip():
                        print(f"{FG_GRAY}(no matching events){RESET}")
                        print()
                else:
                    # Add spacing even when no events
                    print()
            except Exception as e:
                print(f"{FG_RED}Failed to load events: {e}{RESET}")
                print()

            # Extra blank line before status rows for visual separation
            print()

            # Position cursor at bottom for filter/status rows (row = height - 2)
            cols, rows = get_terminal_size()
            target_row = rows - 1  # 0-indexed, so rows-1 is second-to-last row
            sys.stdout.write(f'\033[{target_row};1H')  # Move to target row, column 1
            sys.stdout.flush()

            # Render bottom rows
            self._render_events_bottom(cols, matched_count, total_count)

            return get_last_event_id(), cols, matched_count, total_count

        # Initial draw
        last_pos, last_width, last_matched, last_total = redraw_all()
        last_status_update = time.time()

        with KeyboardInput() as kbd:
            while True:
                key = kbd.get_key()

                # Tab always exits (user requirement)
                if key == 'TAB':
                    sys.stdout.write('\r\033[K')  # Clear status line
                    break

                # Enter cycles event types (all → message → status → life → all)
                elif key == 'ENTER':
                    cycle = {"all": "message", "message": "status", "status": "life", "life": "all"}
                    self.state.event_type_filter = cycle[self.state.event_type_filter]
                    last_pos, last_width, last_matched, last_total = redraw_all()

                # ESC clears filter
                elif key == 'ESC':
                    if self.state.event_filter:
                        self.state.event_filter = ""
                        self.state.event_filter_cursor = 0
                        last_pos, last_width, last_matched, last_total = redraw_all()

                # Backspace deletes char
                elif key == 'BACKSPACE':
                    if self.state.event_filter and self.state.event_filter_cursor > 0:
                        self.state.event_filter = (
                            self.state.event_filter[:self.state.event_filter_cursor-1] +
                            self.state.event_filter[self.state.event_filter_cursor:]
                        )
                        self.state.event_filter_cursor -= 1
                        last_pos, last_width, last_matched, last_total = redraw_all()

                # Printable chars: type-to-activate filtering
                elif key and len(key) == 1 and key.isprintable():
                    sanitized = self.sanitize_filter_input(key)
                    if sanitized:
                        self.state.event_filter = (
                            self.state.event_filter[:self.state.event_filter_cursor] +
                            sanitized +
                            self.state.event_filter[self.state.event_filter_cursor:]
                        )
                        self.state.event_filter_cursor += len(sanitized)
                        last_pos, last_width, last_matched, last_total = redraw_all()

                # Update status periodically
                now = time.time()
                if now - last_status_update > 0.5:
                    current_cols, _ = get_terminal_size()
                    self.load_status()

                    # Check if resize requires redraw
                    if current_cols != last_width:
                        last_pos, last_width, last_matched, last_total = redraw_all()
                    else:
                        # Just update status line
                        sys.stdout.write('\r' + '\033[A\033[K')  # Move up 1 row, clear
                        self._render_events_bottom(current_cols, last_matched, last_total, use_write=True)

                    last_status_update = now
                    last_width = current_cols

                # Stream new events
                from ..core.db import get_last_event_id, get_events_since

                try:
                    current_max_id = get_last_event_id()
                    if current_max_id > last_pos:
                        event_type = None if self.state.event_type_filter == "all" else self.state.event_type_filter
                        events = get_events_since(last_pos, event_type=event_type)

                        if events:
                            # Count new matches
                            new_matches = []
                            for event in events:
                                if self.matches_filter_safe(event, self.state.event_filter):
                                    new_matches.append(event)

                            if new_matches:
                                # Clear bottom rows and render new events
                                sys.stdout.write('\r\033[A\033[K\n\033[K\033[A\r')

                                for event in new_matches:
                                    self.render_event_safe(event)
                                    last_matched += 1

                                last_total += len(events)

                                # Re-render bottom rows
                                cols, _ = get_terminal_size()
                                self._render_events_bottom(cols, last_matched, last_total)

                        last_pos = current_max_id
                except Exception as e:
                    self.flash_error(f"Stream failed: {e}", duration=3.0)

                time.sleep(0.01)

        # Return to TUI
        sys.stdout.write(HIDE_CURSOR + '\033[?1049h')
        sys.stdout.flush()

    def handle_key(self, key: str):
        """Handle key press based on current mode"""
        if self.mode == Mode.MANAGE:
            self.manage_screen.handle_key(key)
        elif self.mode == Mode.LAUNCH:
            self.launch_screen.handle_key(key)
