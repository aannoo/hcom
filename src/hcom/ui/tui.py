"""Main TUI orchestration.

This module contains the HcomTUI class which orchestrates the entire TUI:
- Main event loop (keyboard input, updates, rendering)
- Screen switching (Manage ↔ Launch)
- State management (loading, saving)
- Flash notifications

Architecture
------------
HcomTUI is the coordinator, not a renderer. It:
1. Owns the shared UIState object
2. Creates screen instances (ManageScreen, LaunchScreen)
3. Dispatches keyboard events to the active screen
4. Calls screen.build() to get lines for rendering
5. Handles frame output with differential updates

The run() method is the entry point that:
1. Sets up alternate screen and raw terminal mode
2. Runs the main loop: update() → render() → handle input
3. Cleans up on exit

Terminal Handling
-----------------
- Uses alternate screen buffer (\\033[?1049h/l)
- Hides cursor during operation
- Restores all settings on exit (normal or exception)
"""

import os
import select as select_mod
import shlex
import shutil
import signal
import subprocess
import sys
import time
import tomllib
from pathlib import Path
from typing import Optional

# Import types
from .types import Mode, UIState
from .rendering import (
    ansi_len,
    ansi_ljust,
    truncate_ansi,
    truncate_path,
    get_terminal_size,
    get_message_pulse_colors,
    separator_line,
    suppress_output,
    bg_ljust,
)
from .input import KeyboardInput

# Import ANSI codes directly to avoid circular import
from .colors import (
    RESET,
    BOLD,
    DIM,
    FG_GREEN,
    FG_CYAN,
    FG_WHITE,
    FG_BLACK,
    FG_GRAY,
    FG_YELLOW,
    FG_RED,
    FG_ORANGE,
    FG_LIGHTGRAY,
    FG_BLUE,
    BG_ORANGE,
    CLEAR_SCREEN,
    CURSOR_HOME,
    HIDE_CURSOR,
)

# Import non-color constants from shared
from ..shared import (
    STATUS_ORDER,
    STATUS_BG_MAP,
    STATUS_FG,
    HcomError,
    format_timestamp,
    get_status_counts,
    parse_iso_timestamp,
)
from ..tools.claude import resolve_claude_args
from ..tools.gemini.args import resolve_gemini_args
from ..tools.codex.args import resolve_codex_args
from ..core.instances import get_instance_status
from ..core.paths import hcom_path, ensure_hcom_directories, CONFIG_TOML, ENV_FILE
from ..core.config import (
    reload_config,
    load_config_snapshot,
    save_config,
    dict_to_hcom_config,
    HcomConfigError,
)
from ..commands.lifecycle import cmd_stop
from ..commands.reset import cmd_reset

# Import screens
from .manage import ManageScreen
from .launch import LaunchScreen

# Import config from parent package
from . import CONFIG_DEFAULTS

# TUI Layout Constants
MESSAGE_PREVIEW_LIMIT = 100  # Keep last N messages in message preview


class HcomTUI:
    """Main TUI application orchestrator.

    Coordinates screen rendering, input handling, state management,
    and background tasks (relay sync, status updates).

    This class is the entry point for the TUI. It manages:
    - Terminal setup/teardown (alternate screen, raw mode)
    - Main event loop
    - Screen switching between Manage and Launch modes
    - Shared state (UIState) that screens read and mutate
    - Config file loading/saving
    - Flash notifications

    Attributes:
        hcom_dir: Path to the HCOM data directory (~/.hcom).
        mode: Current screen mode (Mode.MANAGE or Mode.LAUNCH).
        state: Shared UIState object containing all UI state.
        manage_screen: ManageScreen instance for Manage mode.
        launch_screen: LaunchScreen instance for Launch mode.

    Class Constants:
        CONFIRMATION_TIMEOUT: Seconds before two-step confirmations expire.
        CONFIRMATION_FLASH_DURATION: Duration to show confirmation flash.
    """

    # Confirmation timeout constants
    CONFIRMATION_TIMEOUT = 10.0  # State cleared after this
    CONFIRMATION_FLASH_DURATION = 10.0  # Flash duration matches timeout

    def __init__(self, hcom_dir: Path):
        """Initialize the TUI orchestrator.

        Args:
            hcom_dir: Path to HCOM data directory (e.g., ~/.hcom).
        """
        self.hcom_dir = hcom_dir
        self.mode = Mode.MANAGE
        self.state = UIState()  # All shared state in one place

        # Runtime orchestrator fields (not in UIState)
        self.last_frame: list[str] = []
        self.last_status_update = 0.0
        self.last_config_check = 0.0
        self.first_render = True
        self.relay_text_until = 0.0  # Show "connected" text briefly on startup

        # Screen instances (pass state + self)
        self.manage_screen = ManageScreen(self.state, self)
        self.launch_screen = LaunchScreen(self.state, self)

    def flash(self, msg: str, duration: float = 2.0, color: str = "orange"):
        """Show temporary flash notification in the status bar.

        Flash messages appear briefly to confirm actions or show status.
        They automatically disappear after the specified duration.

        Args:
            msg: Message text to display.
            duration: Display time in seconds (default 2.0).
            color: Background color - "red", "white", or "orange" (default).
        """
        self.state.flash_message = msg
        self.state.flash_until = time.time() + duration
        self.state.flash_color = color
        self.state.frame_dirty = True

    def flash_error(self, msg: str, duration: float = 10.0):
        """Show error flash notification with red background.

        Longer duration than regular flash for visibility.

        Args:
            msg: Error message text.
            duration: Display time in seconds (default 10.0).
        """
        self.state.flash_message = msg
        self.state.flash_until = time.time() + duration
        self.state.flash_color = "red"
        self.state.frame_dirty = True

    def clear_all_pending_confirmations(self):
        """Clear all pending confirmation states and flash if any were active"""
        had_pending = (
            self.state.confirm.pending_stop or self.state.confirm.pending_stop_all or self.state.confirm.pending_reset
        )

        self.state.confirm.pending_stop = None
        self.state.confirm.pending_stop_all = False
        self.state.confirm.pending_reset = False

        if had_pending:
            self.state.flash_message = None

    def clear_pending_confirmations_except(self, keep: str):
        """Clear all pending confirmations except the specified one ('stop', 'stop_all', 'reset')"""
        had_pending = False

        if keep != "stop" and self.state.confirm.pending_stop:
            self.state.confirm.pending_stop = None
            had_pending = True
        if keep != "stop_all" and self.state.confirm.pending_stop_all:
            self.state.confirm.pending_stop_all = False
            had_pending = True
        if keep != "reset" and self.state.confirm.pending_reset:
            self.state.confirm.pending_reset = False
            had_pending = True

        if had_pending:
            self.state.flash_message = None

    def stop_all_instances(self):
        """Stop all instances (row exists = participating)"""
        try:
            # Count instances before stopping
            count_before = len(self.state.manage.instances)

            # Suppress CLI output to prevent TUI corruption
            with suppress_output():
                result = cmd_stop(["all"])
            if result == 0:
                self.load_status()
                # Count how many were actually stopped (deleted)
                count_after = len(self.state.manage.instances)
                stopped_count = count_before - count_after

                if stopped_count > 0:
                    self.flash(f"Stopped {stopped_count}")
                else:
                    self.flash("Nothing to stop")
            else:
                self.flash_error("Failed to stop")
        except (HcomError, OSError) as e:
            self.flash_error(f"Error: {str(e)}")

    def reset_events(self):
        """Reset events (archive and clear database)"""
        try:
            # Close stale connection before reset (clear() deletes DB file)
            from ..core.db import close_db

            close_db()

            # Suppress CLI output to prevent TUI corruption
            with suppress_output():
                result = cmd_reset([])
            if result == 0:
                # Clear message state
                self.state.manage.messages = []
                self.state.events.last_event_id = 0
                self.state.manage.device_sync_times = {}  # Clear cached sync times
                self.state.manage.device_short_ids = {}
                # Reload to clear instance list from display
                self.load_status()
                archive_path = str(hcom_path("archive")) + "/"
                self.flash(f"Logs and instance list archived to {archive_path}", duration=10.0)
            else:
                self.flash_error("Failed to reset events")
        except (HcomError, OSError) as e:
            self.flash_error(f"Error: {str(e)}")

    def run(self) -> int:
        """Main event loop - entry point for the TUI.

        Sets up the terminal, runs the main loop, and cleans up on exit.
        This method blocks until the user exits (Ctrl+D).

        The loop:
        1. Checks for pending input (skip update/render if input waiting)
        2. Calls update() to refresh data from DB
        3. Calls render() to output the current frame
        4. Reads keyboard input and dispatches to active screen

        Returns:
            Exit code (0 for normal exit, 1 for error).
        """
        # Set terminal title (OSC 1 = tab, OSC 2 = window)
        sys.stdout.write("\033]1;hcom\007\033]2;hcom\007")
        sys.stdout.flush()

        # Initialize
        ensure_hcom_directories()

        # Load saved states (config.toml first, then launch state reads from it)
        self.load_config_from_file()
        self.load_launch_state()

        # Enter alternate screen
        sys.stdout.write("\033[?1049h")
        sys.stdout.flush()

        # Install SIGWINCH handler for immediate resize response
        old_sigwinch = signal.getsignal(signal.SIGWINCH)
        signal.signal(signal.SIGWINCH, lambda *_: setattr(self.state, "frame_dirty", True))

        try:
            with KeyboardInput() as kbd:
                while True:
                    # Only update/render if no pending input (paste optimization)
                    if not kbd.has_input():
                        self.update()
                        self.render()

                    # Block until input arrives or timeout for next update cycle
                    pulse_active = (
                        self.state.last_message_time > 0
                        and time.time() - self.state.last_message_time < 5.0
                    )
                    timeout = 0.05 if pulse_active else 0.1
                    try:
                        ready = select_mod.select([kbd.fd], [], [], timeout)[0]
                    except (InterruptedError, OSError):
                        continue  # SIGWINCH or other signal — re-loop

                    if not ready:
                        continue  # Timeout — loop will call update/render

                    key = kbd.get_key()
                    if not key:
                        continue

                    if key == "CTRL_D":
                        # Save state before exit
                        self.save_launch_state()
                        break
                    elif key == "TAB":
                        # Save state when switching modes
                        if self.mode == Mode.LAUNCH:
                            self.save_launch_state()
                        # Cancel tag edit if active
                        if self.state.manage.tag_edit_target:
                            self.state.manage.tag_edit_target = None
                            self.state.manage.message_buffer = self.state.manage.tag_edit_original_buffer
                            self.state.manage.tag_edit_original_buffer = ""
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
            sys.stdout.write("\033[?1049l")  # Exit alternate screen
            sys.stdout.write("\033[?25h")  # Show cursor
            sys.stdout.flush()
            # Now write error with traceback
            import traceback

            sys.stderr.write(f"\nError: {e}\n")
            traceback.print_exc()
            return 1
        finally:
            # Restore SIGWINCH handler
            signal.signal(signal.SIGWINCH, old_sigwinch)

            # Ensure terminal restored (idempotent)
            sys.stdout.write("\033[?1049l")
            sys.stdout.write("\033[?25h")
            sys.stdout.flush()

    def load_status(self):
        """Load instance status from DB (streamed, not all at once)"""
        import sqlite3
        from ..core.db import iter_instances, close_db
        from ..core.instances import cleanup_stale_placeholders, cleanup_stale_instances

        try:
            # Clean up stale launching placeholders (>2min) and stale instances (>1hr)
            cleanup_stale_placeholders()
            cleanup_stale_instances()

            # Stream instances from DB
            instances = {}
            for data in iter_instances():
                instances[data["name"]] = data
        except sqlite3.OperationalError as e:
            # DB was deleted/reset by another process - reconnect
            close_db()
            if "locked" in str(e).lower():
                return
            instances = {}
            try:
                for data in iter_instances():
                    instances[data["name"]] = data
            except sqlite3.OperationalError:
                return

        # Build instance info dict (replace old instances, don't just add)
        from ..core.instances import get_full_name

        new_instances = {}
        for name, data in instances.items():
            status_type, age_text, description, age_seconds, _ = get_instance_status(data)

            # Compute full display name ({tag}-{name} or just {name})
            full_name = get_full_name(data)

            new_instances[full_name] = {
                "status": status_type,
                "age_text": age_text,
                "description": description,
                "age_seconds": age_seconds,
                "data": data,
                "base_name": name,  # Keep base name for DB lookups
            }

        self.state.manage.instances = new_instances
        # Status counts for all instances (row exists = participating)
        self.state.manage.status_counts = get_status_counts(self.state.manage.instances)

        # Calculate unread message counts for LOCAL instances only
        # Remote instances have unknown read positions (last_event_id=0 after import)
        try:
            from ..core.messages import get_unread_counts_batch

            # Build dict of {base_name: data} for unread calculation (local only)
            instances_for_unread = {
                info["base_name"]: info["data"]
                for info in new_instances.values()
                if not info["data"].get("origin_device_id")  # Skip remote
            }
            # Get counts keyed by base_name, then map to full_name for display
            base_counts = get_unread_counts_batch(instances_for_unread)
            # Map base_name counts to full_name for TUI display
            self.state.manage.unread_counts = {
                full_name: base_counts.get(info["base_name"], 0)
                for full_name, info in new_instances.items()
                if not info["data"].get("origin_device_id")  # Skip remote
            }
        except Exception:
            pass  # Keep existing counts if calculation fails

        # Load archive count (shown when no instances)
        from ..core.paths import hcom_path, ARCHIVE_DIR

        archive_dir = hcom_path(ARCHIVE_DIR)
        if archive_dir.exists():
            self.state.archive_count = len(list(archive_dir.glob("session-*")))
        else:
            self.state.archive_count = 0

        # Load device sync times and short IDs for remote device display
        try:
            from ..relay import get_remote_devices

            device_times = {}
            device_short_ids = {}
            for device_id, info in get_remote_devices().items():
                if info["sync_time"]:
                    device_times[device_id] = info["sync_time"]
                device_short_ids[device_id] = info["short_id"]
            self.state.manage.device_sync_times = device_times
            self.state.manage.device_short_ids = device_short_ids
        except Exception:
            pass  # Keep existing data if query fails

        # Load relay status for status bar indicator
        try:
            from ..relay import get_relay_status

            status = get_relay_status()
            prev_status = self.state.relay.status
            self.state.relay.configured = status["configured"]
            self.state.relay.enabled = status["enabled"]
            self.state.relay.status = status["status"]
            self.state.relay.error = status["error"]
            # Show "connected" text briefly on startup or when status transitions to ok
            if status["status"] == "ok" and prev_status != "ok":
                self.relay_text_until = time.time() + 5.0
        except (OSError, sqlite3.Error):
            pass

    def save_launch_state(self):
        """Save launch form values to config.toml via args parsers.

        IMPORTANT: All config_edit updates are done first, then a single
        save_config_to_file() call at the end. This avoids the reload loop where
        each save triggers load_launch_state() before other values are saved.
        """
        # Phase 1: Update all config_edit values in memory

        # Save Claude args to HCOM_CLAUDE_ARGS
        try:
            claude_args_str = self.state.config_edit.get("HCOM_CLAUDE_ARGS", "")
            spec = resolve_claude_args(None, claude_args_str if claude_args_str else None)

            # Update spec with background and prompt
            spec = spec.update(
                background=self.state.launch.background,
                prompt=self.state.launch.prompt,  # Always pass value (empty string deletes)
            )

            # Build tokens, filtering out existing system prompts from clean_tokens
            tokens = []
            skip_next = False
            for i, token in enumerate(spec.clean_tokens):
                if skip_next:
                    skip_next = False
                    continue
                token_lower = token.lower()
                # Skip system prompt flags and their values
                if token_lower in ("--system-prompt", "--append-system-prompt"):
                    # Only skip next token if it's not another flag (it's the value)
                    if i + 1 < len(spec.clean_tokens):
                        next_token = spec.clean_tokens[i + 1]
                        if not next_token.startswith("-"):
                            skip_next = True
                    continue
                if token_lower.startswith(("--system-prompt=", "--append-system-prompt=")):
                    continue
                tokens.append(token)

            # Add UI state system prompts
            if self.state.launch.system_prompt:
                tokens.extend(["--system-prompt", self.state.launch.system_prompt])
            if self.state.launch.append_system_prompt:
                tokens.extend(["--append-system-prompt", self.state.launch.append_system_prompt])

            # Re-parse to get proper spec
            spec = resolve_claude_args(tokens, None)
            self.state.config_edit["HCOM_CLAUDE_ARGS"] = spec.to_env_string()
        except Exception as e:
            sys.stderr.write(f"Warning: Failed to save Claude args: {e}\n")

        # Save Gemini prompt to HCOM_GEMINI_ARGS
        try:
            gemini_args_str = self.state.config_edit.get("HCOM_GEMINI_ARGS", "")
            gemini_spec = resolve_gemini_args(None, gemini_args_str if gemini_args_str else None)
            gemini_spec = gemini_spec.update(prompt=self.state.launch.gemini_prompt)
            self.state.config_edit["HCOM_GEMINI_ARGS"] = gemini_spec.to_env_string()
        except Exception as e:
            sys.stderr.write(f"Warning: Failed to save Gemini args: {e}\n")

        # Save Codex prompt to HCOM_CODEX_ARGS
        try:
            codex_args_str = self.state.config_edit.get("HCOM_CODEX_ARGS", "")
            codex_spec = resolve_codex_args(None, codex_args_str if codex_args_str else None)
            codex_spec = codex_spec.update(prompt=self.state.launch.codex_prompt)
            self.state.config_edit["HCOM_CODEX_ARGS"] = codex_spec.to_env_string()
        except Exception as e:
            sys.stderr.write(f"Warning: Failed to save Codex args: {e}\n")

        # Save system prompts to dedicated env vars (Gemini/Codex don't have CLI flags for this)
        self.state.config_edit["HCOM_GEMINI_SYSTEM_PROMPT"] = self.state.launch.gemini_system_prompt
        self.state.config_edit["HCOM_CODEX_SYSTEM_PROMPT"] = self.state.launch.codex_system_prompt

        # Phase 2: Single write to config.toml (triggers load_launch_state() AFTER all values are set)
        self.save_config_to_file()

    def load_launch_state(self):
        """Load launch form values from config.toml via tool args parsers"""
        # Validate Claude args from HCOM_CLAUDE_ARGS
        try:
            claude_args_str = self.state.config_edit.get("HCOM_CLAUDE_ARGS", "")
            spec = resolve_claude_args(None, claude_args_str if claude_args_str else None)

            # Check for parse errors and surface them
            if spec.errors:
                self.state.launch.validation_errors["HCOM_CLAUDE_ARGS"] = spec.errors[0]
            else:
                self.state.launch.validation_errors.pop("HCOM_CLAUDE_ARGS", None)

            # Extract Claude-related fields from spec
            self.state.launch.background = spec.is_background
            self.state.launch.prompt = spec.positional_tokens[0] if spec.positional_tokens else ""

            # Extract both system prompt types
            self.state.launch.system_prompt = spec.get_flag_value("--system-prompt") or ""
            self.state.launch.append_system_prompt = spec.get_flag_value("--append-system-prompt") or ""

            # Clamp cursors to valid range (preserve position if within bounds)
            self.state.launch.prompt_cursor = min(self.state.launch.prompt_cursor, len(self.state.launch.prompt))
            self.state.launch.system_prompt_cursor = min(
                self.state.launch.system_prompt_cursor,
                len(self.state.launch.system_prompt),
            )
            self.state.launch.append_system_prompt_cursor = min(
                self.state.launch.append_system_prompt_cursor,
                len(self.state.launch.append_system_prompt),
            )
        except Exception as e:
            # Failed to parse - use defaults and log warning
            sys.stderr.write(f"Warning: Failed to load Claude args (using defaults): {e}\n")

        # Load Codex prompt from HCOM_CODEX_ARGS
        try:
            codex_args_str = self.state.config_edit.get("HCOM_CODEX_ARGS", "")
            codex_spec = resolve_codex_args(None, codex_args_str if codex_args_str else None)
            if codex_spec.errors:
                self.state.launch.validation_errors["HCOM_CODEX_ARGS"] = codex_spec.errors[0]
            else:
                self.state.launch.validation_errors.pop("HCOM_CODEX_ARGS", None)
            self.state.launch.codex_prompt = codex_spec.positional_tokens[0] if codex_spec.positional_tokens else ""
            self.state.launch.codex_prompt_cursor = min(
                self.state.launch.codex_prompt_cursor,
                len(self.state.launch.codex_prompt),
            )
        except Exception as e:
            sys.stderr.write(f"Warning: Failed to load Codex args: {e}\n")

        # Load Gemini prompt from HCOM_GEMINI_ARGS (-i flag)
        try:
            gemini_args_str = self.state.config_edit.get("HCOM_GEMINI_ARGS", "")
            gemini_spec = resolve_gemini_args(None, gemini_args_str if gemini_args_str else None)
            if gemini_spec.errors:
                self.state.launch.validation_errors["HCOM_GEMINI_ARGS"] = gemini_spec.errors[0]
            else:
                self.state.launch.validation_errors.pop("HCOM_GEMINI_ARGS", None)
            # Read from -i/--prompt-interactive flag (interactive mode with initial prompt)
            prompt_value = gemini_spec.get_flag_value("-i") or gemini_spec.get_flag_value("--prompt-interactive") or ""
            self.state.launch.gemini_prompt = prompt_value if isinstance(prompt_value, str) else ""
            self.state.launch.gemini_prompt_cursor = min(
                self.state.launch.gemini_prompt_cursor,
                len(self.state.launch.gemini_prompt),
            )
        except Exception as e:
            sys.stderr.write(f"Warning: Failed to load Gemini args: {e}\n")

        # Load system prompts from dedicated env vars (Gemini/Codex don't have CLI flags for this)
        self.state.launch.gemini_system_prompt = self.state.config_edit.get("HCOM_GEMINI_SYSTEM_PROMPT", "")
        self.state.launch.gemini_system_prompt_cursor = min(
            self.state.launch.gemini_system_prompt_cursor,
            len(self.state.launch.gemini_system_prompt),
        )
        self.state.launch.codex_system_prompt = self.state.config_edit.get("HCOM_CODEX_SYSTEM_PROMPT", "")
        self.state.launch.codex_system_prompt_cursor = min(
            self.state.launch.codex_system_prompt_cursor,
            len(self.state.launch.codex_system_prompt),
        )

    def load_config_from_file(self, *, raise_on_error: bool = False):
        """Load all vars from config.toml + env into editable dict"""
        config_path = hcom_path(CONFIG_TOML)
        try:
            snapshot = load_config_snapshot()
            combined: dict[str, str] = {}
            combined.update(snapshot.values)
            combined.update(snapshot.extras)
            self.state.config_edit = combined
            self.state.launch.validation_errors.clear()
            # Track mtime for external change detection (both config.toml and env)
            try:
                self.state.config_mtime = config_path.stat().st_mtime
            except FileNotFoundError:
                self.state.config_mtime = 0.0
            try:
                self.state.env_mtime = hcom_path(ENV_FILE).stat().st_mtime
            except FileNotFoundError:
                self.state.env_mtime = 0.0
            LaunchScreen.invalidate_defaults_cache()  # Clear cached defaults
        except (OSError, tomllib.TOMLDecodeError) as e:
            if raise_on_error:
                raise
            sys.stderr.write(f"Warning: Failed to load config.toml (using defaults): {e}\n")
            self.state.config_edit = dict(CONFIG_DEFAULTS)
            self.state.config_mtime = 0.0

    def save_config_to_file(self):
        """Write current config edits back to config.toml + env using canonical writer."""
        known_values = {key: self.state.config_edit.get(key, "") for key in CONFIG_DEFAULTS.keys()}
        extras = {key: value for key, value in self.state.config_edit.items() if key not in CONFIG_DEFAULTS}

        field_map = {
            "timeout": "HCOM_TIMEOUT",
            "subagent_timeout": "HCOM_SUBAGENT_TIMEOUT",
            "terminal": "HCOM_TERMINAL",
            "tag": "HCOM_TAG",
            "claude_args": "HCOM_CLAUDE_ARGS",
            "hints": "HCOM_HINTS",
            "notes": "HCOM_NOTES",
        }

        try:
            core = dict_to_hcom_config(known_values)
        except HcomConfigError as exc:
            self.state.launch.validation_errors.clear()
            for field, message in exc.errors.items():
                env_key = field_map.get(field, field.upper())
                self.state.launch.validation_errors[env_key] = message
            first_error = next(iter(self.state.launch.validation_errors.values()), "Invalid config")
            self.flash_error(first_error)
            return
        except (ValueError, TypeError) as exc:
            self.flash_error(f"Validation error: {exc}")
            return

        try:
            save_config(core, extras)
            self.state.launch.validation_errors.clear()
            self.state.flash_message = None
            # Reload snapshot to pick up canonical formatting
            self.load_config_from_file()
            self.load_launch_state()
            # Refresh runtime config cache (for relay, etc.)
            reload_config()
            # Update relay status in UI state
            self.load_status()
        except OSError as exc:
            self.flash_error(f"Save failed: {exc}")

    def check_external_config_changes(self):
        """Reload config if changed on disk (config.toml or env), preserving active edits."""
        config_path = hcom_path(CONFIG_TOML)
        env_path = hcom_path(ENV_FILE)
        try:
            config_mtime = config_path.stat().st_mtime
        except FileNotFoundError:
            config_mtime = 0.0
        try:
            env_mtime = env_path.stat().st_mtime
        except FileNotFoundError:
            env_mtime = 0.0

        if config_mtime <= self.state.config_mtime and env_mtime <= self.state.env_mtime:
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
            LaunchScreen.invalidate_defaults_cache()  # Clear cached defaults
        except Exception as exc:
            self.flash_error(f"Failed to reload config.toml: {exc}")
            return

        # Update mtimes
        try:
            self.state.config_mtime = config_path.stat().st_mtime
        except FileNotFoundError:
            self.state.config_mtime = 0.0
        try:
            self.state.env_mtime = env_path.stat().st_mtime
        except FileNotFoundError:
            self.state.env_mtime = 0.0

        self.state.frame_dirty = True

        # Restore in-progress edit if field changed externally
        if active_field and active_field[0]:
            key, value, cursor = active_field
            # Check if the field we're editing changed externally
            if key in old_edit and old_edit.get(key) != self.state.config_edit.get(key):
                # External change to field you're editing - keep your version
                self.state.config_edit[key] = value
                if key in self.state.launch.config_field_cursors:
                    self.state.launch.config_field_cursors[key] = cursor
                self.flash(f"Kept in-progress {key} edit (external change ignored)")

    def resolve_editor_command(self) -> tuple[list[str] | None, str | None]:
        """Resolve preferred editor command and display label for config edits."""
        config_path = hcom_path(CONFIG_TOML)
        editor = os.environ.get("VISUAL") or os.environ.get("EDITOR")
        pretty_names = {
            "code": "VS Code",
            "code-insiders": "VS Code Insiders",
            "hx": "Helix",
            "helix": "Helix",
            "nvim": "Neovim",
            "vim": "Vim",
            "nano": "nano",
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
                if normalized.endswith(".exe"):
                    normalized = normalized[:-4]
                label = pretty_names.get(normalized, base_name)
                return parts + [str(config_path)], label

        if code_bin := shutil.which("code"):
            return [code_bin, str(config_path)], "VS Code"
        if nano_bin := shutil.which("nano"):
            return [nano_bin, str(config_path)], "nano"
        if vim_bin := shutil.which("vim"):
            return [vim_bin, str(config_path)], "vim"
        return None, None

    def open_config_in_editor(self):
        """Open config.toml in the resolved editor."""
        cmd, label = self.resolve_editor_command()
        if not cmd:
            self.flash_error("No external editor found")
            return

        # Ensure latest in-memory edits are persisted before handing off
        self.save_config_to_file()

        try:
            subprocess.Popen(cmd)
            self.flash(f"Opening config.toml in {label or 'VS Code'}...")
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
        if self.state.manage.send_state == "sent" and now >= self.state.manage.send_state_until:
            self.state.manage.send_state = None
            self.state.frame_dirty = True

        # Update interval: faster during active pulse animation, slower when idle
        pulse_active = self.state.last_message_time > 0 and now - self.state.last_message_time < 5.0
        update_interval = 0.1 if pulse_active else 0.5

        if now - self.last_status_update >= update_interval:
            self.load_status()
            self.last_status_update = now
            self.state.frame_dirty = True

        # Clear pending stop after timeout
        if self.state.confirm.pending_stop and (now - self.state.confirm.pending_stop_time) > self.CONFIRMATION_TIMEOUT:
            self.state.confirm.pending_stop = None
            self.state.manage.show_instance_detail = None
            self.state.frame_dirty = True

        # Clear pending stop all after timeout
        if (
            self.state.confirm.pending_stop_all
            and (now - self.state.confirm.pending_stop_all_time) > self.CONFIRMATION_TIMEOUT
        ):
            self.state.confirm.pending_stop_all = False
            self.state.frame_dirty = True

        # Clear pending reset after timeout
        if (
            self.state.confirm.pending_reset
            and (now - self.state.confirm.pending_reset_time) > self.CONFIRMATION_TIMEOUT
        ):
            self.state.confirm.pending_reset = False
            self.state.frame_dirty = True

        # Periodic config reload check (detects external changes from CLI, editor, etc.)
        if (now - self.last_config_check) >= 0.5:
            self.last_config_check = now
            self.check_external_config_changes()

        # Load messages for MANAGE screen preview (with event ID caching)
        if self.mode == Mode.MANAGE:
            from ..core.db import get_last_event_id, get_events_since

            try:
                current_max_id = get_last_event_id()
                # Detect external reset: max ID dropped means DB was cleared
                if current_max_id < self.state.events.last_event_id:
                    self.state.manage.messages = []
                    self.state.events.last_event_id = 0
                    self.state.frame_dirty = True
                if current_max_id != self.state.events.last_event_id:
                    events = get_events_since(self.state.events.last_event_id, event_type="message")
                    from ..core.instances import get_full_name, load_instance_position

                    new_messages = []
                    for e in events:
                        event_data = e["data"]  # Already a dict from db.py
                        # Convert sender base name to full display name
                        sender_base = event_data.get("from", "")
                        sender_data = load_instance_position(sender_base) if sender_base else None
                        sender_display = get_full_name(sender_data) or sender_base
                        # Convert recipient base names to full display names
                        delivered_to_base = event_data.get("delivered_to", [])
                        delivered_to = []
                        for r_base in delivered_to_base:
                            r_data = load_instance_position(r_base)
                            delivered_to.append(get_full_name(r_data) or r_base)
                        new_messages.append(
                            (
                                e["timestamp"],
                                sender_display,
                                event_data.get("text", ""),
                                delivered_to,
                                e["id"],  # event_id for read receipt lookup
                            )
                        )

                    # Append new messages and keep last N
                    all_messages = list(self.state.manage.messages) + new_messages
                    self.state.manage.messages = (
                        all_messages[-MESSAGE_PREVIEW_LIMIT:]
                        if len(all_messages) > MESSAGE_PREVIEW_LIMIT
                        else all_messages
                    )

                    # Update last message time for EVENTS tab pulse
                    if all_messages:
                        last_msg_timestamp = all_messages[-1][0]
                        dt = parse_iso_timestamp(last_msg_timestamp) if "T" in last_msg_timestamp else None
                        self.state.last_message_time = dt.timestamp() if dt else 0.0
                    else:
                        self.state.last_message_time = 0.0

                    self.state.events.last_event_id = current_max_id
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

        # Build path indicator (show which database is active)
        active_path = hcom_path()
        try:
            global_path = Path.home() / ".hcom"
        except Exception:
            global_path = None
        if global_path and active_path == global_path:
            path_indicator = ""  # Don't show when using default ~/.hcom
        else:
            path_indicator = f" {DIM}[{active_path}]{RESET}"

        # Calculate message pulse colors for EVENTS tab
        if self.state.last_message_time > 0:
            seconds_since_msg = time.time() - self.state.last_message_time
        else:
            seconds_since_msg = 9999.0  # No messages yet - use quiet state
        log_bg_color, log_fg_color = get_message_pulse_colors(seconds_since_msg)

        # Build status display (inline foreground-colored counts, no bg blocks)
        is_manage_selected = highlight_tab == "MANAGE"
        status_parts_selected = []
        status_parts_inactive = []

        for status_type in STATUS_ORDER:
            count = self.state.manage.status_counts.get(status_type, 0)
            if count > 0:
                _, symbol = STATUS_BG_MAP[status_type]
                fg_color = STATUS_FG.get(status_type, FG_WHITE)
                # Selected: bold + semantic color. Inactive: dim semantic color.
                status_parts_selected.append(f"{BOLD}{fg_color}{count} {symbol}{RESET}")
                status_parts_inactive.append(f"{DIM}{fg_color}{count} {symbol}{RESET}")

        # Build tabs: MANAGE, LAUNCH, and EVENTS
        tab_names = ["MANAGE", "LAUNCH", "EVENTS"]
        tabs = []

        for tab_name in tab_names:
            if tab_name == "MANAGE":
                if is_manage_selected:
                    manage_label = " ".join(status_parts_selected) if status_parts_selected else f"{FG_ORANGE}{BOLD}MANAGE{RESET}"
                else:
                    manage_label = " ".join(status_parts_inactive) if status_parts_inactive else f"{FG_GRAY}MANAGE{RESET}"
                tabs.append(f" {manage_label}")
            elif tab_name == highlight_tab:
                # Selected non-MANAGE tab: orange text, bold
                tabs.append(f" {FG_ORANGE}{BOLD}{tab_name}{RESET}")
            elif tab_name == "EVENTS" and seconds_since_msg < 5.0:
                # EVENTS tab with recent activity: pulse colors (white→dim fade)
                tabs.append(f"{log_bg_color}{log_fg_color} {tab_name} {RESET}")
            else:
                # Inactive tab: dim text, no background
                tabs.append(f" {FG_GRAY}{tab_name}{RESET}")

        tab_display = " ".join(tabs)

        # Relay indicator - only show if configured AND enabled
        relay_indicator = ""
        if self.state.relay.configured and self.state.relay.enabled:
            if self.state.relay.status == "error":
                icon = f"{FG_RED}⇄{RESET}"
                err = self.state.relay.error
                relay_indicator = f"  {icon} {err}" if err else f"  {icon}"
            elif self.state.relay.status == "ok":
                icon = f"{FG_GREEN}⇄{RESET}"
                # Show "relay connected" text briefly after connect
                if time.time() < self.relay_text_until:
                    relay_indicator = f"  {icon} {FG_GREEN}relay connected{RESET}"
                else:
                    relay_indicator = f"  {icon}"
            else:
                # Never connected yet
                icon = f"{FG_GRAY}⇄{RESET}"
                relay_indicator = f"  {icon}"

        return f"{BOLD}hcom{RESET} {tab_display}{relay_indicator}{path_indicator}"

    def build_flash(self) -> Optional[str]:
        """Build flash notification if active"""
        if self.state.flash_message and time.time() < self.state.flash_until:
            color_map = {"red": FG_RED, "white": FG_WHITE, "orange": FG_ORANGE}
            color_code = color_map.get(self.state.flash_color, FG_ORANGE)
            cols, _ = get_terminal_size()
            # Reserve space for "• " prefix and separator/padding
            max_msg_width = cols - 10
            msg = (
                truncate_ansi(self.state.flash_message, max_msg_width)
                if len(self.state.flash_message) > max_msg_width
                else self.state.flash_message
            )
            return f"{BOLD}{color_code}• {msg}{RESET}"
        return None

    # Layout: left/right margin so content doesn't hug terminal edges
    MARGIN = 0

    def render(self):
        """Render current screen"""
        # Skip rebuild if nothing changed
        if not self.state.frame_dirty:
            return

        cols, rows = get_terminal_size()
        # Adapt to any terminal size
        rows = max(10, rows)
        # Content width: leave margin on left and right
        cw = cols - self.MARGIN * 2
        pad = " " * self.MARGIN

        frame = []

        # Header (compact - no separator)
        header = self.build_status_bar()
        frame.append(f"{pad}{ansi_ljust(header, cw)}")

        # Flash row with separator line
        flash = self.build_flash()
        if flash:
            # Flash message on left, separator line fills rest of row
            flash_len = ansi_len(flash)
            remaining = cw - flash_len - 1  # -1 for space
            sep = separator_line(remaining) if remaining > 0 else ""
            frame.append(f"{pad}{flash} {sep}")
        else:
            # Just separator line when no flash message
            frame.append(f"{pad}{separator_line(cw)}")

        # Welcome message on first render
        if self.first_render:
            self.flash("Welcome! Tab to switch screens")
            self.first_render = False

        # Body (subtract 3: header, flash, footer)
        body_rows = rows - 3

        if self.mode == Mode.MANAGE:
            manage_lines = self.manage_screen.build(body_rows, cw)
            for line in manage_lines:
                frame.append(f"{pad}{ansi_ljust(line, cw)}")
        elif self.mode == Mode.LAUNCH:
            form_lines = self.launch_screen.build(body_rows, cw)
            for line in form_lines:
                frame.append(f"{pad}{ansi_ljust(line, cw)}")

        # Footer - compact help text
        footer = ""
        if self.mode == Mode.MANAGE:
            # Contextual footer based on state
            if self.state.manage.tag_edit_target:
                footer = f"{FG_GRAY}enter: save tag  esc: cancel{RESET}"
            elif self.state.manage.message_buffer.strip():
                footer = f"{FG_GRAY}tab: switch  @: mention  enter: send  esc: clear{RESET}"
            elif self.state.confirm.pending_stop_all:
                footer = f"{FG_GRAY}ctrl+k: confirm stop all  esc: cancel{RESET}"
            elif self.state.confirm.pending_reset:
                footer = f"{FG_GRAY}ctrl+r: confirm clear  esc: cancel{RESET}"
            elif self.state.confirm.pending_stop:
                # Row exists = participating, can only stop
                footer = f"{FG_GRAY}enter: stop  esc: cancel{RESET}"
            else:
                footer = (
                    f"{FG_GRAY}tab: switch  @: mention  enter: view details  ctrl+k: stop all  ctrl+r: clear{RESET}"
                )
        elif self.mode == Mode.LAUNCH:
            footer = self.launch_screen.get_footer()
        frame.append(f"{pad}{truncate_ansi(footer, cw)}")

        # Repaint if changed
        if frame != self.last_frame:
            sys.stdout.write(CLEAR_SCREEN + CURSOR_HOME)
            for i, line in enumerate(frame):
                sys.stdout.write(line)
                if i < len(frame) - 1:
                    sys.stdout.write("\n")
            sys.stdout.flush()
            self.last_frame = frame

        # Frame rebuilt - clear dirty flag
        self.state.frame_dirty = False

    def handle_tab(self):
        """Cycle between Manage, Launch, and native Log view"""
        if self.mode == Mode.MANAGE:
            self.mode = Mode.LAUNCH
        elif self.mode == Mode.LAUNCH:
            # Go directly to native events view instead of alternate mode
            self.show_events_native()
            # After returning from native view, go to MANAGE
            self.mode = Mode.MANAGE

    def sanitize_filter_input(self, text: str) -> str:
        """Remove dangerous chars, limit length for filter input"""
        # Strip control chars except printable
        cleaned = "".join(c for c in text if c.isprintable() or c in " \t")
        # Truncate to prevent paste bombs
        return cleaned[:200]

    def matches_filter(self, event: dict, query: str) -> bool:
        """Check if event matches query (multi-word AND). May raise KeyError/TypeError."""
        if not query or not query.strip():
            return True  # Empty query = show all

        # Split query into words (AND logic)
        words = [w.casefold() for w in query.split()]

        # Build searchable string from all event fields
        data = event.get("data", "")
        if isinstance(data, dict):
            # Extract values from dict for better searchability
            data_str = " ".join(str(v) for v in data.values())
        elif isinstance(data, str):
            data_str = data
        else:
            data_str = str(data)

        searchable = (event.get("type", "") + " " + event.get("instance", "") + " " + data_str).casefold()

        # All words must match (AND)
        return all(word in searchable for word in words)

    def matches_filter_safe(self, event: dict, query: str) -> bool:
        """Match event against query with error boundary"""
        try:
            return self.matches_filter(event, query)
        except (KeyError, TypeError, AttributeError, UnicodeDecodeError) as e:
            # Malformed event or encoding issue - treat as non-match
            import sys

            print(
                f"DEBUG: Event {event.get('id', '?')} match failed: {e}",
                file=sys.stderr,
            )
            return False

    def render_event(self, event: dict):
        """Render event by type with defensive defaults.
        Format: time name: [type] content
        """
        pad = " " * self.MARGIN
        for line in self._format_event_lines_inner(event):
            print(f"{pad}{line}")

    def render_event_safe(self, event: dict):
        """Render event with fallback for malformed data"""
        pad = " " * self.MARGIN
        try:
            self.render_event(event)
        except Exception:
            event_id = event.get("id", "?")
            print(f"{pad}{FG_GRAY}[malformed event {event_id}]{RESET}")
            print()

    def _format_event_lines(self, event: dict) -> list[str]:
        """Format event as list of lines (for buffered output). Same format as render_event."""
        try:
            return self._format_event_lines_inner(event)
        except Exception:
            event_id = event.get("id", "?")
            return [f"{FG_GRAY}[malformed event {event_id}]{RESET}", ""]

    def _format_event_lines_inner(self, event: dict) -> list[str]:
        """Format event as list of lines."""
        event_type = event.get("type", "unknown")
        timestamp = event.get("timestamp", "")
        instance = event.get("instance", "?")
        data = event.get("data", {})

        type_labels = {"message": "message", "status": "status", "life": "life"}
        type_label = type_labels.get(event_type, event_type)
        lines = []

        if event_type == "message":
            sender = data.get("from", "?")
            message = data.get("text", "")
            display_time = format_timestamp(timestamp)

            intent = data.get("intent")
            thread = data.get("thread")
            reply_to = data.get("reply_to")

            if intent or thread or reply_to:
                intent_colors = {
                    "request": FG_ORANGE,
                    "inform": FG_LIGHTGRAY,
                    "ack": FG_GREEN,
                    "error": FG_RED,
                }
                intent_color = intent_colors.get(intent, FG_GRAY)
                parts = []
                if intent:
                    parts.append(f"{intent_color}{intent}{RESET}")
                if thread:
                    t = thread[:12] + ".." if len(thread) > 14 else thread
                    parts.append(f"{DIM}→ {RESET}{FG_BLUE}{t}{RESET}")
                if reply_to:
                    parts.append(f"{DIM}↩ {FG_LIGHTGRAY}{reply_to}{RESET}")
                envelope = f"{DIM}[{RESET}{' '.join(parts)}{DIM}]{RESET}"
            else:
                envelope = f"{DIM}[{type_label}]{RESET}"

            lines.append(f"{DIM}{FG_GRAY}{display_time}{RESET} {BOLD}{FG_ORANGE}{sender}{RESET} {envelope}")
            # Split message on newlines so each line is a separate entry
            lines.extend(message.split("\n"))
            lines.append("")

        elif event_type == "status":
            status = data.get("status", "?")
            context = data.get("context", "")
            detail = data.get("detail", "")
            ctx = f", {context}" if context else ""
            if detail:
                ctx += f": {truncate_path(detail, 60)}"
            lines.append(
                f"{DIM}{FG_GRAY}{format_timestamp(timestamp)}{RESET} "
                f"{BOLD}{FG_CYAN}{instance}{RESET}: {FG_GRAY}[{type_label}]{RESET} {status}{ctx}"
            )
            lines.append("")

        elif event_type == "life":
            action = data.get("action", "?")
            lines.append(
                f"{DIM}{FG_GRAY}{format_timestamp(timestamp)}{RESET} "
                f"{BOLD}{FG_YELLOW}{instance}{RESET}: {FG_GRAY}[{type_label}]{RESET} {action}"
            )
            # Show snapshot details for stopped events
            if action == "stopped":
                reason = data.get("reason", "")
                by = data.get("by", "")
                snapshot = data.get("snapshot", {})
                tool = snapshot.get("tool", "")
                directory = snapshot.get("directory", "")
                session_id = snapshot.get("session_id", "")
                transcript = snapshot.get("transcript_path", "")
                pid = snapshot.get("pid")
                # Always show these
                lines.append(f"      {FG_GRAY}by: {by}  reason: {reason}  tool: {tool}  pid: {pid}{RESET}")
                lines.append(f"      {FG_GRAY}dir: {truncate_path(directory, 50)}{RESET}")
                lines.append(f"      {FG_GRAY}session: {session_id}{RESET}")
                lines.append(f"      {FG_GRAY}transcript: {truncate_path(transcript, 120)}{RESET}")
                # Conditionally show optional fields
                optional = []
                if snapshot.get("tag"):
                    optional.append(f"tag: {snapshot['tag']}")
                if snapshot.get("hints"):
                    optional.append(f"hints: {snapshot['hints'][:30]}")
                if snapshot.get("parent_name"):
                    optional.append(f"parent: {snapshot['parent_name']}")
                if snapshot.get("agent_id"):
                    optional.append(f"agent_id: {snapshot['agent_id']}")
                if snapshot.get("subagent_timeout"):
                    optional.append(f"subagent_timeout: {snapshot['subagent_timeout']}")
                if snapshot.get("launch_args") and snapshot["launch_args"] != "[]":
                    optional.append(f"launch_args: {snapshot['launch_args'][:30]}")
                if snapshot.get("background_log_file"):
                    optional.append(f"log: {truncate_path(snapshot['background_log_file'], 40)}")
                if optional:
                    lines.append(f"      {FG_GRAY}{' | '.join(optional)}{RESET}")
            lines.append("")

        else:
            lines.append(
                f"{DIM}{FG_GRAY}{format_timestamp(timestamp)}{RESET} "
                f"{BOLD}{instance}{RESET}: {FG_GRAY}[{event_type}]{RESET} {data}"
            )
            lines.append("")

        return lines

    def _render_events_bottom(self, cols: int, matched: int, total: int, use_write: bool = False):
        """Render bottom rows for events view (filter/separator + status bar)

        Pads all output to full width to avoid flicker (overwrite instead of clear).
        """
        cw = cols - self.MARGIN * 2
        pad = " " * self.MARGIN
        text_filter_active = bool(self.state.events.filter.strip())
        type_filter_active = self.state.events.type_filter != "all"

        # First row: filter line or separator/flash
        if text_filter_active or type_filter_active:
            # Build filter line with optional type chip + text query
            parts = ["Filter: "]

            # Type chip when not 'all' (match event format: [type])
            if type_filter_active:
                type_chip = f"[{self.state.events.type_filter}]"
                parts.append(type_chip)
                parts.append(" ")  # Space after chip

            # Text query with cursor
            if text_filter_active or type_filter_active:
                available = cw - 30  # Reserve space for prefix, chip, count
                filter_text = (
                    self.state.events.filter[:available]
                    if len(self.state.events.filter) > available
                    else self.state.events.filter
                )
                cursor_display = "_" if self.state.events.filter_cursor == len(self.state.events.filter) else ""
                filter_display = (
                    filter_text[: self.state.events.filter_cursor]
                    + cursor_display
                    + filter_text[self.state.events.filter_cursor :]
                )
                parts.append(filter_display)

            count_str = f" [{matched}/{total}]"
            parts.append(count_str)
            # Fill remaining space with separator line (like flash line)
            filter_content = "".join(parts)
            filter_len = ansi_len(filter_content)
            remaining = cw - filter_len - 1
            sep = separator_line(remaining) if remaining > 0 else ""
            first_line = f"{pad}{filter_content} {sep}"
        else:
            flash = self.build_flash()
            if flash:
                flash_len = ansi_len(flash)
                remaining = cw - flash_len - 1
                sep = separator_line(remaining) if remaining > 0 else ""
                first_line = f"{pad}{flash} {sep}"
            else:
                first_line = f"{pad}{separator_line(cw)}"

        # Output first line (already full width from separator or ljust)
        if use_write:
            sys.stdout.write(first_line + "\n")
        else:
            print(first_line)

        # Status bar with footer hints (match MANAGE/LAUNCH format)
        # Build archive indicator
        archive_indicator = ""
        if self.state.events.archive_mode and self.state.events.archive_index:
            archive_indicator = f"  {FG_ORANGE}[Archive {self.state.events.archive_index}]{RESET}"

        # Build hints based on current view
        if self.state.events.archive_picker:
            view_suffix = f"  {FG_GRAY}↑↓: select  enter: view  esc: back{RESET}"
        elif self.state.events.instances_view:
            view_suffix = f"  {FG_GRAY}↑↓: select  enter: filter  esc: back{RESET}"
        elif self.state.events.archive_mode:
            # Archive mode: show esc to return to live (clear only if text filter)
            if self.state.events.filter:
                view_suffix = f"  {FG_GRAY}←→: type  esc: clear/live  ctrl+g: archives{RESET}"
            else:
                view_suffix = f"  {FG_GRAY}←→: type  esc: live  ctrl+g: archives{RESET}"
        else:
            # Live mode: show g if archives exist (clear only if text filter)
            has_archives = bool(self.state.archive_count > 0)
            esc_hint = "  esc: clear" if self.state.events.filter else ""
            if has_archives:
                view_suffix = f"  {FG_GRAY}←→: type  enter: instances{esc_hint}  ctrl+g: archives{RESET}"
            else:
                view_suffix = f"  {FG_GRAY}←→: type  enter: instances{esc_hint}{RESET}"
        status = self.build_status_bar(highlight_tab="EVENTS") + archive_indicator + view_suffix
        # Pad to full width to overwrite old content without clearing
        sys.stdout.write(f"{pad}{ansi_ljust(truncate_ansi(status, cw), cw)}")
        sys.stdout.flush()

    def _build_events_bottom_lines(self, cols: int, matched: int, total: int) -> list[str]:
        """Build bottom rows as list of strings (for buffered output)."""
        cw = cols - self.MARGIN * 2
        pad = " " * self.MARGIN
        text_filter_active = bool(self.state.events.filter.strip())
        type_filter_active = self.state.events.type_filter != "all"

        # First row: filter line or separator/flash
        if text_filter_active or type_filter_active:
            parts = ["Filter: "]
            if type_filter_active:
                parts.append(f"[{self.state.events.type_filter}] ")
            if text_filter_active or type_filter_active:
                available = cw - 30
                filter_text = (
                    self.state.events.filter[:available]
                    if len(self.state.events.filter) > available
                    else self.state.events.filter
                )
                cursor_display = "_" if self.state.events.filter_cursor == len(self.state.events.filter) else ""
                filter_display = (
                    filter_text[: self.state.events.filter_cursor]
                    + cursor_display
                    + filter_text[self.state.events.filter_cursor :]
                )
                parts.append(filter_display)
            parts.append(f" [{matched}/{total}]")
            filter_content = "".join(parts)
            remaining = cw - ansi_len(filter_content) - 1
            sep = separator_line(remaining) if remaining > 0 else ""
            first_line = f"{pad}{filter_content} {sep}"
        else:
            flash = self.build_flash()
            if flash:
                remaining = cw - ansi_len(flash) - 1
                sep = separator_line(remaining) if remaining > 0 else ""
                first_line = f"{pad}{flash} {sep}"
            else:
                first_line = f"{pad}{separator_line(cw)}"

        # Second row: status bar
        archive_indicator = ""
        if self.state.events.archive_mode and self.state.events.archive_index:
            archive_indicator = f"  {FG_ORANGE}[Archive {self.state.events.archive_index}]{RESET}"

        if self.state.events.archive_picker:
            view_suffix = f"  {FG_GRAY}↑↓: select  enter: view  esc: back{RESET}"
        elif self.state.events.instances_view:
            view_suffix = f"  {FG_GRAY}↑↓: select  enter: filter  esc: back{RESET}"
        elif self.state.events.archive_mode:
            if self.state.events.filter:
                view_suffix = f"  {FG_GRAY}←→: type  esc: clear/live  ctrl+g: archives{RESET}"
            else:
                view_suffix = f"  {FG_GRAY}←→: type  esc: live  ctrl+g: archives{RESET}"
        else:
            has_archives = bool(self.state.archive_count > 0)
            esc_hint = "  esc: clear" if self.state.events.filter else ""
            if has_archives:
                view_suffix = f"  {FG_GRAY}←→: type  enter: instances{esc_hint}  ctrl+g: archives{RESET}"
            else:
                view_suffix = f"  {FG_GRAY}←→: type  enter: instances{esc_hint}{RESET}"

        status = self.build_status_bar(highlight_tab="EVENTS") + archive_indicator + view_suffix
        second_line = f"{pad}{ansi_ljust(truncate_ansi(status, cw), cw)}"

        return [first_line, second_line]

    def _get_instance_lifecycle_data(self) -> list[dict]:
        """Get instance lifecycle data from life events + current instances table.

        Returns list of dicts with: name, started, stopped (None if running)
        Sorted by started time descending (most recent first).
        """
        from ..core.db import get_db

        conn = get_db()

        # Get all life events to build instance history
        rows = conn.execute("""
            SELECT instance, timestamp, json_extract(data, '$.action') as action
            FROM events
            WHERE type = 'life'
            ORDER BY id
        """).fetchall()

        # Build per-instance lifecycle
        # Track first started and last stopped for each instance
        instances_data = {}  # name -> {started: str, stopped: str|None}

        for row in rows:
            name = row["instance"]
            action = row["action"]
            ts = row["timestamp"]

            if name not in instances_data:
                instances_data[name] = {"name": name, "started": None, "stopped": None}

            # Use 'created' or 'ready' as start time (whichever comes first)
            if action in ("created", "ready"):
                if instances_data[name]["started"] is None:
                    instances_data[name]["started"] = ts
                instances_data[name]["stopped"] = None  # Reset stopped on new start
            elif action == "stopped":
                instances_data[name]["stopped"] = ts

        # Check which instances are currently running (in instances table)
        current_rows = conn.execute("SELECT name FROM instances").fetchall()
        current_names = {row["name"] for row in current_rows}

        # Mark running instances as not stopped
        for name in current_names:
            if name in instances_data:
                instances_data[name]["stopped"] = None
            else:
                # Instance exists but no life events (shouldn't happen, but handle it)
                instances_data[name] = {"name": name, "started": None, "stopped": None}

        # Convert to list and sort into 3 tiers:
        # 1. Alive (in instances table) - sorted by started time descending
        # 2. Actually stopped (has stop timestamp) - sorted by stopped time descending
        # 3. No stop time recorded (not alive, no stop event) - at the bottom
        result = list(instances_data.values())
        alive = [x for x in result if x["name"] in current_names]
        stopped = [x for x in result if x["name"] not in current_names and x["stopped"] is not None]
        no_stop = [x for x in result if x["name"] not in current_names and x["stopped"] is None]
        alive.sort(key=lambda x: x["started"] or "", reverse=True)
        stopped.sort(key=lambda x: x["stopped"] or "", reverse=True)
        no_stop.sort(key=lambda x: x["started"] or "", reverse=True)
        return alive + stopped + no_stop

    def _get_archive_events(self, event_type: str | None = None) -> list[dict]:
        """Get events from currently selected archive.

        Returns events in same format as get_events_since().
        """
        from ..commands.query import _query_archive_events

        if not self.state.events.archive_list or self.state.events.archive_index == 0:
            return []

        # Find archive by index
        archive = None
        for a in self.state.events.archive_list:
            if a["index"] == self.state.events.archive_index:
                archive = a
                break

        if not archive:
            return []

        # Build SQL filter for event type
        sql_filter = None
        if event_type:
            sql_filter = f"type = '{event_type}'"

        try:
            events = _query_archive_events(archive, sql_filter, last=0)  # 0 = no limit
            return events
        except Exception:
            return []

    def _get_archive_instance_lifecycle_data(self) -> list[dict]:
        """Get instance lifecycle data from currently selected archive.

        Returns same format as _get_instance_lifecycle_data().
        """
        from ..commands.query import _query_archive_events

        if not self.state.events.archive_list or self.state.events.archive_index == 0:
            return []

        # Find archive by index
        archive = None
        for a in self.state.events.archive_list:
            if a["index"] == self.state.events.archive_index:
                archive = a
                break

        if not archive:
            return []

        try:
            # Get life events from archive
            events = _query_archive_events(archive, "type = 'life'", last=0)

            # Build per-instance lifecycle (same logic as _get_instance_lifecycle_data)
            instances_data = {}
            for event in events:
                name = event["instance"]
                action = event["data"].get("action")
                ts = event["timestamp"]

                if name not in instances_data:
                    instances_data[name] = {
                        "name": name,
                        "started": None,
                        "stopped": None,
                    }

                if action == "created":
                    instances_data[name]["started"] = ts
                    instances_data[name]["stopped"] = None
                elif action == "stopped":
                    instances_data[name]["stopped"] = ts

            # Archives are historical - sort by stopped time descending (most recent stop first)
            # Fall back to started time if no stopped time
            result = list(instances_data.values())
            result.sort(key=lambda x: x["stopped"] or x["started"] or "", reverse=True)
            return result
        except Exception:
            return []

    def _render_archive_picker(self, cols: int, rows: int) -> tuple[int, int, int, int]:
        """Render archive picker view (uses alt screen).

        Returns (last_pos, cols, matched_count, total_count) for consistency with redraw_all.
        """
        cw = cols - self.MARGIN * 2
        pad = " " * self.MARGIN
        archives = self.state.events.archive_list
        total = len(archives)

        if not archives:
            print(f"{pad}{FG_GRAY}(no archives found){RESET}")
            print()
            return 0, cols, 0, 0

        # Clamp cursor
        if self.state.events.archive_cursor >= len(archives):
            self.state.events.archive_cursor = len(archives) - 1
        if self.state.events.archive_cursor < 0:
            self.state.events.archive_cursor = 0

        # Calculate visible window (header=2, status=2, buffer=1)
        max_visible = rows - 5
        if max_visible < 1:
            max_visible = 1

        # Calculate viewport
        cursor = self.state.events.archive_cursor
        if len(archives) <= max_visible:
            start_idx = 0
            end_idx = len(archives)
        else:
            half = max_visible // 2
            start_idx = max(0, cursor - half)
            end_idx = start_idx + max_visible
            if end_idx > len(archives):
                end_idx = len(archives)
                start_idx = end_idx - max_visible

        visible = archives[start_idx:end_idx]

        # Header with scroll indicator
        scroll_indicator = ""
        if start_idx > 0:
            scroll_indicator = f" {FG_GRAY}↑{start_idx} more{RESET}"
        print(f"{pad}{BOLD}Select Archive:{RESET}{scroll_indicator}")
        print()

        displayed = 0
        for idx, archive in enumerate(visible):
            actual_idx = start_idx + idx
            is_selected = actual_idx == self.state.events.archive_cursor

            # Format: index. name  events  instances
            name = archive["name"]
            events = archive.get("events", "?")
            instances = archive.get("instances", "?")

            # Truncate name if needed
            max_name_len = cw - 30
            if len(name) > max_name_len:
                name = name[: max_name_len - 3] + "..."

            content = f" {archive['index']:>2}. {name}  {events} events  {instances} instances"

            if is_selected:
                row = f"{pad}{BG_ORANGE}{FG_BLACK}{content:<{cw - 1}}{RESET}"
            else:
                row = f"{pad}{FG_GRAY}{content}{RESET}"
            print(row)
            displayed += 1

        # Scroll indicator at bottom (include 50 limit note if applicable)
        remaining = len(archives) - end_idx
        if remaining > 0:
            limit_note = "  (use 'hcom archive' for full list)" if len(archives) >= 50 else ""
            print(f"{pad}{FG_GRAY}↓{remaining} more{limit_note}{RESET}")
        elif len(archives) >= 50:
            print(f"{pad}{FG_GRAY}(use 'hcom archive' for full list){RESET}")
        else:
            print()

        # Position cursor at bottom for separator + status bar
        sys.stdout.write(f"\033[{rows - 1};1H\033[K")  # Row for separator
        sys.stdout.write(f"{pad}{separator_line(cw)}\n")
        sys.stdout.write("\033[K")  # Clear status line

        # Render status bar for picker mode
        status = self.build_status_bar(highlight_tab="EVENTS")
        hints = f"  {FG_GRAY}↑↓: select  enter: view  esc: back{RESET}"
        sys.stdout.write(f"{pad}{truncate_ansi(status + hints, cw)}")
        sys.stdout.flush()

        return 0, cols, displayed, total

    def _render_instances_view(self, cols: int, rows: int = 24) -> tuple[int, int]:
        """Render instances summary view. Returns (displayed_count, total_count)."""
        from ..shared import format_age

        cw = cols - self.MARGIN * 2
        pad = " " * self.MARGIN

        # Use archive or live data source
        if self.state.events.archive_mode:
            instances = self._get_archive_instance_lifecycle_data()
        else:
            instances = self._get_instance_lifecycle_data()
        total = len(instances)

        if not instances:
            self.state.events.instances_list = []
            self.state.events.instances_data = []
            if self.state.events.archive_mode:
                print(f"{pad}{FG_GRAY}(archive empty){RESET}")
            else:
                print(f"{pad}{FG_GRAY}(none in session){RESET}")
            print()
            return 0, 0

        # Most recent first (at top) - natural for alt screen interactive view

        # Store instance names and data for cursor navigation
        self.state.events.instances_list = [i["name"] for i in instances]
        self.state.events.instances_data = instances

        # Clamp cursor to valid range
        if self.state.events.instances_cursor >= len(instances):
            self.state.events.instances_cursor = len(instances) - 1
        if self.state.events.instances_cursor < 0:
            self.state.events.instances_cursor = 0

        # Calculate visible window (header=2, status=2, query_hint=1, buffer=1)
        max_visible = rows - 6
        if max_visible < 1:
            max_visible = 1

        # Calculate viewport to keep cursor visible
        cursor = self.state.events.instances_cursor
        if len(instances) <= max_visible:
            # All fit
            start_idx = 0
            end_idx = len(instances)
        else:
            # Scroll to keep cursor in view
            half = max_visible // 2
            start_idx = max(0, cursor - half)
            end_idx = start_idx + max_visible
            if end_idx > len(instances):
                end_idx = len(instances)
                start_idx = end_idx - max_visible

        visible_instances = instances[start_idx:end_idx]

        # Calculate column widths
        max_name = max(len(i["name"]) for i in instances)
        name_width = min(max(max_name, 8), 20)  # 8-20 chars

        # Header
        header = f"{BOLD}{'NAME':<{name_width}}  {'STARTED':<16}  STOPPED{RESET}"
        print(f"{pad}{truncate_ansi(header, cw)}")
        if start_idx > 0:
            print(f"{pad}{FG_GRAY}↑{start_idx} more{RESET}")
        else:
            print()

        # Get current live instances from MANAGE for color logic
        live_instances = set(self.state.manage.instances.keys())

        # Fixed column widths
        started_width = 20
        stopped_width = 20

        displayed = 0
        for idx, inst in enumerate(visible_instances):
            actual_idx = start_idx + idx
            name_display = inst["name"][:name_width]
            is_selected = actual_idx == self.state.events.instances_cursor

            # Format started time
            if inst["started"]:
                started_dt = parse_iso_timestamp(inst["started"])
                if started_dt:
                    local_dt = started_dt.astimezone()
                    started_str = local_dt.strftime("%H:%M") + f" ({format_age(time.time() - started_dt.timestamp())})"
                else:
                    started_str = inst["started"][:16]
            else:
                started_str = "-"

            # Format stopped time
            if inst["stopped"]:
                stopped_dt = parse_iso_timestamp(inst["stopped"])
                if stopped_dt:
                    local_dt = stopped_dt.astimezone()
                    stopped_str = local_dt.strftime("%H:%M") + f" ({format_age(time.time() - stopped_dt.timestamp())})"
                else:
                    stopped_str = inst["stopped"][:16]
            else:
                stopped_str = "-"

            # Color: green if in current MANAGE instances, grey otherwise
            is_live = inst["name"] in live_instances
            color = FG_GREEN if is_live else FG_GRAY

            # Build row content with fixed widths
            content = f"{name_display:<{name_width}}  {started_str:<{started_width}}  {stopped_str:<{stopped_width}}"

            # Cursor highlight: orange bg stretches full row width
            if is_selected:
                row = f"{BG_ORANGE}{FG_BLACK}{content}{RESET}"
                row = truncate_ansi(row, cw)
                row = bg_ljust(row, cw, BG_ORANGE)
            else:
                row = f"{color}{content}{RESET}"
                row = truncate_ansi(row, cw)
            print(f"{pad}{row}")
            displayed += 1

        # Show more indicator at bottom
        remaining = len(instances) - end_idx
        if remaining > 0:
            print(f"{pad}{FG_GRAY}↓{remaining} more{RESET}")
        else:
            print()

        print()

        return displayed, total

    def show_events_native(self, start_in_instances_view: bool = False):
        """Exit TUI, show streaming events in native buffer with filtering support

        Args:
            start_in_instances_view: If True, start in instances view instead of events view
        """
        # Clear filter on entry (fresh start each time)
        self.state.events.filter = ""
        self.state.events.filter_cursor = 0
        self.state.events.instances_view = start_in_instances_view  # Start in specified view
        self.state.events.instances_cursor = 0  # Reset cursor

        # Clear archive state on entry (start with live events)
        self.state.events.archive_mode = False
        self.state.events.archive_picker = False
        self.state.events.archive_index = 0
        self.state.events.archive_cursor = 0

        # Clear flash on entry (was causing perceived input delay)
        self.state.flash_message = None

        # Exit alt screen (cursor handled by KeyboardInput context)
        sys.stdout.write("\033[?1049l")
        sys.stdout.flush()

        def redraw_all():
            """Redraw entire event list with filtering (on entry or resize)"""
            from ..core.db import get_events_since, get_last_event_id

            # Get terminal size upfront
            cols, rows = get_terminal_size()

            # Initialize counts
            matched_count = 0
            total_count = 0

            # Archive picker view (uses alt screen)
            if self.state.events.archive_picker:
                sys.stdout.write("\033[?1049h")  # Enter alt screen
                sys.stdout.write("\033[2J\033[H")  # Clear and home
                sys.stdout.flush()
                return self._render_archive_picker(cols, rows)

            # Instances view (uses alt screen)
            elif self.state.events.instances_view:
                sys.stdout.write("\033[?1049h")  # Enter alt screen
                sys.stdout.write("\033[2J\033[H")  # Clear and home
                sys.stdout.flush()
                matched_count, total_count = self._render_instances_view(cols, rows)

            # Events view (uses native buffer for scrollback)
            else:
                # Buffer all output to write atomically (prevents flash)
                pad = " " * self.MARGIN
                output_lines = []

                try:
                    event_type = None if self.state.events.type_filter == "all" else self.state.events.type_filter

                    # Get events from archive or live db
                    if self.state.events.archive_mode:
                        events = self._get_archive_events(event_type)
                    else:
                        events = get_events_since(0, event_type=event_type)

                    total_count = len(events)

                    if events:
                        # Filter and render all matching events to buffer
                        for event in events:
                            if self.matches_filter_safe(event, self.state.events.filter):
                                output_lines.extend(f"{pad}{line}" for line in self._format_event_lines(event))
                                matched_count += 1

                        # Show message if no matches when filtering
                        if matched_count == 0 and self.state.events.filter.strip():
                            output_lines.append(f"{pad}{FG_GRAY}(no matching events) - Esc to clear | ←→ change type{RESET}")
                            output_lines.append("")
                    else:
                        # Show appropriate message
                        if self.state.events.archive_mode:
                            output_lines.append(f"{pad}{FG_GRAY}(no events in archive){RESET}")
                        output_lines.append("")
                except Exception as e:
                    output_lines.append(f"{pad}{FG_RED}Failed to load events: {e}{RESET}")
                    output_lines.append("")

                # Build bottom rows into buffer too (prevents flash between events and status)
                # Extra blank before status - matches streaming which adds blank after events
                output_lines.append("")
                output_lines.append("")

                # Position cursor at bottom and add status rows to buffer
                target_row = rows - 1
                bottom_lines = self._build_events_bottom_lines(cols, matched_count, total_count)

                # Write EVERYTHING in one flush: exit-alt + clear + events + position + status
                full_output = "\033[?1049l\033[2J\033[H" + "\n".join(output_lines)
                full_output += f"\033[{target_row};1H" + "\n".join(bottom_lines)
                sys.stdout.write(full_output)
                sys.stdout.flush()

            return get_last_event_id(), cols, matched_count, total_count

        # Enter cbreak mode BEFORE initial draw so keypresses aren't lost
        with KeyboardInput() as kbd:
            # Initial draw
            last_pos, last_width, last_matched, last_total = redraw_all()
            last_status_update = time.time()

            while True:
                key = kbd.get_key()

                # Tab always exits (user requirement)
                if key == "TAB":
                    # Exit alt screen if in instances view or archive picker
                    if self.state.events.instances_view or self.state.events.archive_picker:
                        sys.stdout.write("\033[?1049l")
                    sys.stdout.write("\r\033[K")  # Clear status line
                    break

                # Ctrl+G: open archive picker
                elif key == "CTRL_G":
                    from ..commands.query import _list_archives

                    archives = _list_archives(limit=50)
                    if archives:
                        self.state.events.archive_list = archives
                        self.state.events.archive_picker = True
                        self.state.events.archive_cursor = 0
                        last_pos, last_width, last_matched, last_total = redraw_all()
                    # else: no archives - could flash a message but for now just ignore

                # Enter behavior depends on view
                elif key == "ENTER":
                    if self.state.events.archive_picker:
                        # In archive picker: select archive → view its events
                        if self.state.events.archive_list:
                            selected = self.state.events.archive_list[self.state.events.archive_cursor]
                            self.state.events.archive_index = selected["index"]
                            self.state.events.archive_mode = True
                            self.state.events.archive_picker = False
                            self.state.events.filter = ""
                            self.state.events.filter_cursor = 0
                    elif self.state.events.instances_view:
                        # In instances view: select instance → filter events by name
                        if self.state.events.instances_list:
                            selected_name = self.state.events.instances_list[self.state.events.instances_cursor]
                            self.state.events.filter = selected_name
                            self.state.events.filter_cursor = len(selected_name)
                            self.state.events.instances_view = False  # Switch to events
                    else:
                        # In events view: go to instances view (clear filters)
                        self.state.events.filter = ""
                        self.state.events.filter_cursor = 0
                        self.state.events.type_filter = "all"
                        self.state.events.instances_view = True
                    last_pos, last_width, last_matched, last_total = redraw_all()

                # Up/Down arrows for cursor navigation in picker/instances view
                elif key == "UP":
                    if self.state.events.archive_picker and self.state.events.archive_list:
                        self.state.events.archive_cursor = max(0, self.state.events.archive_cursor - 1)
                        last_pos, last_width, last_matched, last_total = redraw_all()
                    elif self.state.events.instances_view and self.state.events.instances_list:
                        self.state.events.instances_cursor = max(0, self.state.events.instances_cursor - 1)
                        last_pos, last_width, last_matched, last_total = redraw_all()
                elif key == "DOWN":
                    if self.state.events.archive_picker and self.state.events.archive_list:
                        max_idx = len(self.state.events.archive_list) - 1
                        self.state.events.archive_cursor = min(max_idx, self.state.events.archive_cursor + 1)
                        last_pos, last_width, last_matched, last_total = redraw_all()
                    elif self.state.events.instances_view and self.state.events.instances_list:
                        max_idx = len(self.state.events.instances_list) - 1
                        self.state.events.instances_cursor = min(max_idx, self.state.events.instances_cursor + 1)
                        last_pos, last_width, last_matched, last_total = redraw_all()

                # Left/Right arrows cycle event types (only in events view, not picker)
                elif key == "RIGHT":
                    if not self.state.events.instances_view and not self.state.events.archive_picker:
                        cycle = {
                            "all": "message",
                            "message": "status",
                            "status": "life",
                            "life": "all",
                        }
                        self.state.events.type_filter = cycle[self.state.events.type_filter]
                        last_pos, last_width, last_matched, last_total = redraw_all()
                elif key == "LEFT":
                    if not self.state.events.instances_view and not self.state.events.archive_picker:
                        cycle = {
                            "all": "life",
                            "life": "status",
                            "status": "message",
                            "message": "all",
                        }
                        self.state.events.type_filter = cycle[self.state.events.type_filter]
                        last_pos, last_width, last_matched, last_total = redraw_all()

                # ESC: context-dependent back/clear action
                elif key == "ESC":
                    if self.state.events.archive_picker:
                        # In archive picker: cancel and return to live events
                        self.state.events.archive_picker = False
                        last_pos, last_width, last_matched, last_total = redraw_all()
                    elif self.state.events.instances_view:
                        # In instances view: go back to events
                        self.state.events.instances_view = False
                        last_pos, last_width, last_matched, last_total = redraw_all()
                    elif self.state.events.archive_mode:
                        # In archive mode: first ESC clears filter, second returns to live
                        if self.state.events.filter:
                            self.state.events.filter = ""
                            self.state.events.filter_cursor = 0
                        else:
                            self.state.events.archive_mode = False
                            self.state.events.archive_index = 0
                        last_pos, last_width, last_matched, last_total = redraw_all()
                    elif self.state.events.filter:
                        # In live events: clear filter
                        self.state.events.filter = ""
                        self.state.events.filter_cursor = 0
                        last_pos, last_width, last_matched, last_total = redraw_all()

                # Backspace deletes char
                elif key == "BACKSPACE":
                    if self.state.events.filter and self.state.events.filter_cursor > 0:
                        self.state.events.filter = (
                            self.state.events.filter[: self.state.events.filter_cursor - 1]
                            + self.state.events.filter[self.state.events.filter_cursor :]
                        )
                        self.state.events.filter_cursor -= 1
                        last_pos, last_width, last_matched, last_total = redraw_all()

                # Printable chars: type-to-activate filtering
                elif key and len(key) == 1 and key.isprintable():
                    sanitized = self.sanitize_filter_input(key)
                    if sanitized:
                        self.state.events.filter = (
                            self.state.events.filter[: self.state.events.filter_cursor]
                            + sanitized
                            + self.state.events.filter[self.state.events.filter_cursor :]
                        )
                        self.state.events.filter_cursor += len(sanitized)
                        last_pos, last_width, last_matched, last_total = redraw_all()

                # Update status periodically
                now = time.time()
                if now - last_status_update > 0.5:
                    current_cols, _ = get_terminal_size()
                    self.load_status()

                    # Check if resize requires redraw
                    if current_cols != last_width:
                        last_pos, last_width, last_matched, last_total = redraw_all()
                    elif not self.state.events.archive_picker and not self.state.events.instances_view:
                        # Just update status line (skip for picker/instances - they have own rendering)
                        # Position only, no clear - padded output overwrites old content
                        sys.stdout.write("\r\033[A")
                        self._render_events_bottom(current_cols, last_matched, last_total, use_write=True)

                    last_status_update = now
                    last_width = current_cols

                # Stream new events (only in live events view)
                if (
                    not self.state.events.archive_mode
                    and not self.state.events.archive_picker
                    and not self.state.events.instances_view
                ):
                    from ..core.db import get_last_event_id, get_events_since

                    try:
                        current_max_id = get_last_event_id()
                        if current_max_id > last_pos:
                            event_type = (
                                None if self.state.events.type_filter == "all" else self.state.events.type_filter
                            )
                            events = get_events_since(last_pos, event_type=event_type)

                            if events:
                                # Count new matches
                                new_matches = []
                                for event in events:
                                    if self.matches_filter_safe(event, self.state.events.filter):
                                        new_matches.append(event)

                                if new_matches:
                                    # Clear bottom 2 lines (separator + status) before rendering
                                    # Without clearing, event content overwrites them before scrolling
                                    sys.stdout.write("\r\033[A\033[K\n\033[K\033[A")

                                    for event in new_matches:
                                        self.render_event_safe(event)
                                        last_matched += 1

                                    last_total += len(events)

                                    # Extra blank line before status (matches initial render line 1875)
                                    # Without this, status bar overwrites last event's trailing blank after scroll
                                    print()

                                    # Position to bottom and re-render status rows
                                    cols, rows = get_terminal_size()
                                    target_row = rows - 1
                                    sys.stdout.write(f"\033[{target_row};1H")
                                    self._render_events_bottom(cols, last_matched, last_total)

                            last_pos = current_max_id
                    except Exception as e:
                        self.flash_error(f"Stream failed: {e}", duration=3.0)

                time.sleep(0.01)

        # Return to TUI - enter alt screen and clear immediately to avoid black flash
        sys.stdout.write(HIDE_CURSOR + "\033[?1049h\033[2J\033[H")
        sys.stdout.flush()
        # Force immediate redraw (main loop will render on next iteration)
        self.state.frame_dirty = True

    def handle_key(self, key: str):
        """Handle key press based on current mode"""
        if self.mode == Mode.MANAGE:
            result = self.manage_screen.handle_key(key)
            # Handle mode switch signals from manage screen
            if isinstance(result, tuple) and result[0] == "switch_events":
                opts = result[1] if len(result) > 1 else {}
                # Switch to events mode with options
                start_in_instances = opts.get("view") == "instances"
                self.show_events_native(start_in_instances_view=start_in_instances)
                # After returning from native view, stay in MANAGE
        elif self.mode == Mode.LAUNCH:
            self.launch_screen.handle_key(key)
