"""PTY wrapper for external tools - keeps child alive, enables text injection via TCP.

Usage:
    wrapper = PTYWrapper(['gemini'], instance_name='gemini-mapsa')
    wrapper.run()  # Blocks until child exits

Inject text from another process:
    printf 'your message\r' | nc localhost <PORT>

Note: This module requires Unix-only APIs (ptyprocess, termios) and is not available on Windows.
"""

import sys
import re

# Platform guard - PTY requires Unix-only APIs
if sys.platform == "win32":
    raise ImportError(
        "PTY wrapper requires Unix-only APIs (ptyprocess, termios) and is not available on Windows. "
        "Use 'hcom N claude' (hooks mode) instead."
    )

import os
import tty
import errno
import termios
import select
import selectors
import socket
import threading
import signal
import struct
import fcntl
import time
import atexit
from pathlib import Path
from typing import Any, Callable, cast

import ptyprocess
import pyte


class ScreenStabilityTracker:
    """Dirty-based screen stability detection.

    Uses pyte's `screen.dirty` set instead of hashing screen.display:
    - 34,000x faster (checking set vs hashing 4800 cells)
    - Catches color-only changes (shimmer animation marks dirty)
    - Lower CPU overhead (~0.001% vs ~3.4% per instance)
    """

    def __init__(self, stability_seconds: float = 1.0):
        """Initialize tracker.

        Args:
            stability_seconds: Seconds without screen changes required for stability.
        """
        self._stability_seconds = stability_seconds
        self._last_change: float = time.monotonic()
        self._lock = threading.Lock()

    def check_dirty(self, screen: pyte.Screen, instance_name: str = "") -> set[int] | None:
        """Check and clear screen.dirty, updating last_change if dirty.

        Returns:
            Set of dirty row indices before clearing, or None if nothing dirty.
            Caller can use this for cache invalidation.
        """
        snapshot: set[int] | None = None
        with self._lock:
            if screen.dirty:
                # Safe copy with retries if set changed during iteration
                for _ in range(3):
                    try:
                        snapshot = set(screen.dirty)
                        break
                    except RuntimeError:
                        continue
                else:
                    snapshot = set()  # Give up, return empty
                screen.dirty.clear()
                self._last_change = time.monotonic()

        # Log outside lock (I/O shouldn't hold lock)
        if snapshot:
            from ..core.log import log_info

            log_info("pty", "stability.dirty", instance=instance_name, lines=len(snapshot))
        return snapshot if snapshot else None

    def is_stable(self, seconds: float | None = None, instance_name: str = "") -> bool:
        """Check if screen has been stable for the specified duration.

        Args:
            seconds: Override stability window. If None, uses default from __init__.
            instance_name: For logging.
        """
        with self._lock:
            window = seconds if seconds is not None else self._stability_seconds
            elapsed = time.monotonic() - self._last_change
            stable = elapsed >= window
        from ..core.log import log_info

        log_info(
            "pty",
            "stability.check",
            instance=instance_name,
            result=stable,
            elapsed=round(elapsed, 2),
            required=window,
        )
        return stable

    def clear(self) -> None:
        """Reset last_change to now (e.g., after injection)."""
        with self._lock:
            self._last_change = time.monotonic()


class PTYWrapper:
    """PTY wrapper that keeps child alive and accepts text injection via TCP."""

    # OSC9 notification patterns (Codex emits these when approval needed)
    OSC9_APPROVAL = b"\x1b]9;Approval requested"  # exec or MCP elicitation
    OSC9_EDIT = b"\x1b]9;Codex wants to edit"  # file edits

    # Filter out terminal title escape sequences (OSC 0/1/2) from child output
    # Claude Code sets these dynamically based on conversation - we override with our own
    _TITLE_ESCAPE_RE = re.compile(rb"\x1b\][012];[^\x07]*\x07")

    def __init__(
        self,
        command: list[str],
        instance_name: str | None = None,
        tool: str | None = None,
        port: int = 0,
        on_output: Callable[[bytes], None] | None = None,
        on_input: Callable[[bytes], None] | None = None,
        user_activity_cooldown: float = 0.5,
        ready_pattern: bytes | None = None,
        on_ready: Callable[[int], None] | None = None,
    ):
        """Initialize PTY wrapper.

        Args:
            command: Command to run (e.g., ['gemini'])
            instance_name: Optional instance name for logging
            tool: Tool identifier ("claude", "gemini", "codex") for prompt parsing
            port: TCP port for injection (0 = auto-assign)
            on_output: Optional callback called with each output chunk: on_output(data: bytes)
            on_input: Optional callback called with each stdin chunk: on_input(data: bytes)
            user_activity_cooldown: Seconds after last keystroke before safe to inject (default 0.5)
            ready_pattern: Bytes pattern to detect ready state in output (e.g., b'? for shortcuts')
            on_ready: Optional callback called when PTY is ready: on_ready(actual_port: int)
        """
        self.command = command
        self.instance_name = instance_name
        self.tool = tool
        self.port = port  # 0 = auto-assign
        self.on_output = on_output  # Callback for output monitoring
        self.on_input = on_input  # Callback for input monitoring
        self.user_activity_cooldown = user_activity_cooldown
        self.ready_pattern = ready_pattern
        self._on_ready_callback = on_ready  # Callback when PTY is ready

        # Debug mode (enabled via HCOM_PTY_DEBUG env var)
        self._debug_enabled = os.environ.get("HCOM_PTY_DEBUG") == "1"
        self._debug_counter = 0
        self._debug_file = None
        self._debug_last_periodic_dump = 0.0
        if self._debug_enabled:
            # Try to use hcom logs dir first, fallback to temp
            try:
                hcom_dir = os.environ.get("HCOM_DIR", str(Path.home() / ".hcom"))
                debug_dir = Path(hcom_dir) / ".tmp" / "logs" / "pty_debug"
                debug_dir.mkdir(parents=True, exist_ok=True)
                debug_path = debug_dir / f"{instance_name or 'unknown'}_{os.getpid()}.log"
            except Exception:
                import tempfile

                debug_path = (
                    Path(tempfile.gettempdir()) / f"hcom_pty_debug_{instance_name or 'unknown'}_{os.getpid()}.log"
                )

            try:
                self._debug_file = open(debug_path, "w")
                # Can't call _debug_log yet (it's defined later), write directly
                self._debug_file.write(f"PTY Debug log started for {instance_name}\n")
                self._debug_file.write(f"Debug file: {debug_path}\n")
                self._debug_file.write("Will dump screen state every 5 seconds\n")
                self._debug_file.flush()
            except Exception as e:
                print(f"Failed to open debug file: {e}", file=sys.stderr)
                self._debug_enabled = False

        self._ptyproc: ptyprocess.PtyProcess | None = None
        self.pid: int | None = None
        self.orig_termios: list[Any] | None = None
        self.running = False
        self.server_socket: socket.socket | None = None
        self.actual_port: int | None = None
        self.last_user_input: float = 0.0  # Timestamp of last stdin activity
        self._last_idle_time: float = 0.0  # Timestamp when status last became "listening"
        self._output_buffer: bytes = b""  # Rolling buffer for pattern detection
        self._waiting_approval: bool = False  # True when OSC9 approval notification detected
        self._last_output_time: float = time.monotonic()  # Track when last output received
        self._resize_timer: threading.Timer | None = None  # Debounce resize signals

        # Decode ready pattern once for screen scanning
        self._ready_pattern_str: str | None = None
        if self.ready_pattern:
            try:
                self._ready_pattern_str = self.ready_pattern.decode("utf-8", "ignore")
            except Exception:
                pass

        # Cache for ready pattern row (sparse buffer optimization)
        self._cached_ready_row: int | None = None

        # pyte terminal emulation for screen tracking
        rows, cols = self.get_terminal_size()
        self._screen = pyte.Screen(cols, rows)
        self._stream = pyte.ByteStream(self._screen)  # ByteStream handles incremental UTF-8

        # Snapshot-based stability tracking
        self._stability_tracker = ScreenStabilityTracker(stability_seconds=1.0)

        # Unified selector event loop (Phase 2.1)
        self._sel: selectors.BaseSelector | None = None
        self._wake_r: int = -1
        self._wake_w: int = -1
        self._inject_clients: dict[socket.socket, bytearray] = {}

        # Prompt check logging debounce (only log on state change)
        self._last_prompt_check_result: bool | None = None
        self._last_prompt_check_reason: str | None = None

        # Register cleanup
        atexit.register(self.cleanup)

    @property
    def master_fd(self) -> int | None:
        """File descriptor for PTY master (for compatibility with existing code)."""
        return self._ptyproc.fd if self._ptyproc else None

    def get_terminal_size(self) -> tuple[int, int]:
        """Get current terminal size (rows, cols)."""
        try:
            s = struct.pack("HHHH", 0, 0, 0, 0)
            result = fcntl.ioctl(sys.stdout.fileno(), termios.TIOCGWINSZ, s)
            rows, cols, _, _ = struct.unpack("hhhh", result)
            return rows, cols
        except (OSError, AttributeError):
            return 24, 80  # Fallback

    def set_pty_size(self, rows: int, cols: int) -> None:
        """Set PTY size."""
        if self._ptyproc:
            try:
                self._ptyproc.setwinsize(rows, cols)
                # Also resize pyte screen
                self._screen.resize(rows, cols)
            except (OSError, ptyprocess.PtyProcessError):
                pass

    def set_terminal_title(self) -> None:
        """Set terminal window and tab title to the instance name."""
        if self.instance_name:
            try:
                title = f"hcom: {self.instance_name}"
                # Write OSC escape sequence directly to stdout to update title
                # OSC 1 = tab/icon, OSC 2 = window
                sys.stdout.write(f"\x1b]1;{title}\x07\x1b]2;{title}\x07")
                sys.stdout.flush()
            except Exception:
                pass

    def handle_sigwinch(self, _signum, _frame) -> None:
        """Forward terminal resize to child (debounced)."""
        # Cancel pending resize timer
        if self._resize_timer is not None:
            self._resize_timer.cancel()
        # Schedule resize after 50ms of no further signals
        self._resize_timer = threading.Timer(0.05, self._apply_resize)
        self._resize_timer.daemon = True
        self._resize_timer.start()

    def _apply_resize(self) -> None:
        """Actually apply the terminal resize to the PTY."""
        if not self.running:
            return
        self._resize_timer = None
        rows, cols = self.get_terminal_size()
        self.set_pty_size(rows, cols)

    def handle_signal(self, _signum, _frame) -> None:
        """Handle SIGTERM/SIGINT gracefully."""
        self.running = False
        # Wake selector immediately
        if self._wake_w >= 0:
            try:
                os.write(self._wake_w, b"x")
            except OSError:
                pass

    def _setup_injection_server(self) -> bool:
        """Set up non-blocking TCP server for injection. Returns True on success."""
        sock = None
        try:
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            sock.bind(("127.0.0.1", self.port))
            sock.listen(5)
            sock.setblocking(False)
            self.server_socket = sock
            self.actual_port = sock.getsockname()[1]
            return True
        except Exception as e:
            if sock is not None:
                try:
                    sock.close()
                except Exception:
                    pass
            print(f"[pty-wrapper] Listener setup failed: {e}", file=sys.stderr)
            return False

    def _accept_injection(self) -> None:
        """Accept new injection connection (non-blocking)."""
        if not self.server_socket or not self._sel:
            return
        try:
            conn, _ = self.server_socket.accept()
            conn.setblocking(False)
            self._inject_clients[conn] = bytearray()
            self._sel.register(conn, selectors.EVENT_READ, "inject_read")
        except BlockingIOError:
            pass  # No pending connections
        except OSError as e:
            if self.running:
                print(f"[pty-wrapper] Accept error: {e}", file=sys.stderr)

    def _read_injection(self, conn: socket.socket) -> None:
        """Read from injection client (non-blocking), finish on EOF."""
        if conn not in self._inject_clients:
            return
        try:
            while True:
                try:
                    chunk = conn.recv(8192)
                    if not chunk:
                        # EOF - client closed connection
                        self._finish_injection(conn)
                        return
                    self._inject_clients[conn].extend(chunk)
                except BlockingIOError:
                    return  # Would block, wait for more data
        except OSError:
            self._finish_injection(conn)

    def _finish_injection(self, conn: socket.socket) -> None:
        """Process accumulated injection data and inject to PTY."""
        if not self._sel:
            return
        try:
            self._sel.unregister(conn)
        except (KeyError, ValueError):
            pass

        data = bytes(self._inject_clients.pop(conn, b""))
        try:
            conn.close()
        except Exception:
            pass

        if data and self._ptyproc:
            try:
                text = data.decode("utf-8", "surrogateescape")
            except Exception:
                text = data.decode("latin-1")

            # Preserve explicit CRs; strip only a single trailing LF from nc/echo
            if text.endswith("\n"):
                text = text[:-1]
            if text:
                os.write(self._ptyproc.fd, text.encode("utf-8", "surrogateescape"))

    def _render_line_from_buffer(self, row_idx: int) -> str:
        """Render single line from buffer without building full display."""
        row_data: dict[int, Any] = self._screen.buffer.get(row_idx, {})
        # Build line with proper column ordering
        line = [" "] * self._screen.columns
        for col, char in row_data.items():
            if 0 <= col < self._screen.columns:
                line[col] = char.data
        return "".join(line)

    def _scan_for_ready_pattern(self) -> bool:
        """Scan buffer for ready pattern (O(1) cached, O(populated rows) on miss)."""
        if not self._ready_pattern_str:
            return True

        try:
            # Check cached row first (O(1) check)
            if self._cached_ready_row is not None:
                if self._cached_ready_row in self._screen.buffer:
                    line = self._render_line_from_buffer(self._cached_ready_row)
                    if self._ready_pattern_str in line:
                        return True
                # Cache invalid, clear it
                self._cached_ready_row = None

            # Full scan only if cache miss
            # Copy keys to avoid RuntimeError if buffer mutated during iteration
            for row_idx in list(self._screen.buffer.keys()):
                line = self._render_line_from_buffer(row_idx)
                if self._ready_pattern_str in line:
                    self._cached_ready_row = row_idx
                    return True
        except (IndexError, KeyError, AttributeError, RuntimeError):
            # RuntimeError: dict changed size during iteration (race with _stream.feed)
            pass
        return False

    def _invalidate_caches_for_dirty(self, dirty_rows: set[int] | None) -> None:
        """Invalidate cached row values if their rows were modified.

        Called after check_dirty() returns dirty rows.
        """
        if dirty_rows is None:
            return
        # Invalidate ready row cache if that row changed
        if self._cached_ready_row is not None and self._cached_ready_row in dirty_rows:
            self._cached_ready_row = None

    def _update_output_buffer(self, data: bytes) -> None:
        """Update rolling output buffer, pyte screen, and detect patterns."""
        self._output_buffer = (self._output_buffer + data)[-4096:]  # 4KB buffer
        self._last_output_time = time.monotonic()

        # Feed to pyte terminal emulator (ByteStream handles UTF-8 internally)
        try:
            self._stream.feed(data)
        except Exception:
            pass  # Don't break on malformed sequences

        # Check dirty rows and invalidate caches
        dirty_rows = self._stability_tracker.check_dirty(self._screen, self.instance_name or "")
        self._invalidate_caches_for_dirty(dirty_rows)

        # Detect OSC9 approval notifications
        if self.OSC9_APPROVAL in self._output_buffer or self.OSC9_EDIT in self._output_buffer:
            self._waiting_approval = True

    def _handle_pty_output(self) -> bool:
        """Handle PTY output. Returns False on EOF/error."""
        if not self._ptyproc:
            return False
        try:
            data = os.read(self._ptyproc.fd, 1024)
            if not data:
                return False
            # Strip terminal title sequences from child (Claude sets these dynamically)
            filtered = self._TITLE_ESCAPE_RE.sub(b"", data)
            os.write(sys.stdout.fileno(), filtered)
            self._update_output_buffer(data)  # Keep original for pattern detection
            if self.on_output:
                try:
                    self.on_output(data)
                except Exception:
                    pass
            return True
        except OSError as e:
            if e.errno == errno.EIO:
                return False
            raise

    def _handle_stdin(self) -> bool:
        """Handle stdin input. Returns False on EOF/error."""
        if not self._ptyproc:
            return False
        try:
            data = os.read(sys.stdin.fileno(), 1024)
            if not data:
                return False
            self.last_user_input = time.time()
            self._waiting_approval = False
            if self.on_input:
                try:
                    self.on_input(data)
                except Exception:
                    pass
            os.write(self._ptyproc.fd, data)
            return True
        except OSError:
            return False

    def io_loop(self) -> None:
        """Unified selector-based I/O loop.

        Blocks on selector with no periodic wakeups. Handles:
        - PTY output → stdout
        - stdin → PTY (interactive mode)
        - TCP injection connections
        - Wake pipe for shutdown
        """
        if not self._sel or not self._ptyproc:
            return

        stdin_fd = sys.stdin.fileno()
        stdin_registered = False

        # Register PTY
        self._sel.register(self._ptyproc.fd, selectors.EVENT_READ, "pty")

        # Register stdin if TTY
        if os.isatty(stdin_fd):
            try:
                self._sel.register(stdin_fd, selectors.EVENT_READ, "stdin")
                stdin_registered = True
            except (ValueError, OSError):
                pass

        # Register injection server
        if self.server_socket:
            self._sel.register(self.server_socket, selectors.EVENT_READ, "inject_accept")

        # Register wake pipe
        self._sel.register(self._wake_r, selectors.EVENT_READ, "wake")

        # Use shorter timeout when debug enabled for periodic screen dumps
        select_timeout = 5.0 if self._debug_enabled else None

        while self.running and self._ptyproc and self._ptyproc.isalive():
            try:
                events = self._sel.select(timeout=select_timeout)
            except (ValueError, OSError):
                break

            # Periodic screen dump when debug enabled (on timeout with no events)
            if self._debug_enabled and not events:
                if time.time() - self._debug_last_periodic_dump >= 5.0:
                    self._debug_dump_screen("Periodic dump (io_loop)")
                    self._debug_last_periodic_dump = time.time()
                continue

            for key, _ in events:
                if key.data == "wake":
                    # Shutdown signal - drain and exit
                    try:
                        os.read(self._wake_r, 1)
                    except OSError:
                        pass
                    return

                elif key.data == "pty":
                    if not self._handle_pty_output():
                        return

                elif key.data == "stdin":
                    if not self._handle_stdin():
                        # EOF on stdin - unregister but keep running
                        if stdin_registered:
                            try:
                                self._sel.unregister(stdin_fd)
                            except (KeyError, ValueError):
                                pass
                            stdin_registered = False

                elif key.data == "inject_accept":
                    self._accept_injection()

                elif key.data == "inject_read":
                    self._read_injection(cast(socket.socket, key.fileobj))

    def is_user_active(self) -> bool:
        """Check if user is actively typing (within cooldown period).

        Returns False if:
        - No PTY (headless/background mode - no user to be active)
        - Cooldown period has passed since last keystroke
        """
        # No PTY = not active
        if not self._ptyproc or not self._ptyproc.isalive():
            return False

        # No TTY = no human = not active (headless/background)
        try:
            if not os.isatty(sys.stdin.fileno()):
                return False
        except (OSError, ValueError):
            return False  # stdin closed/invalid = no user

        # Check if within cooldown period
        return (time.time() - self.last_user_input) < self.user_activity_cooldown

    def is_ready(self) -> bool:
        """Check if CLI is ready for input injection.

        Scans pyte screen for ready pattern visibility. Pattern disappears when:
        - User types in input box (uncommitted input)
        - Slash menu or other overlay is shown

        Returns:
            True if ready_pattern is currently visible on screen.
            Always returns True if no ready_pattern configured (no gating).
        """
        if not self.ready_pattern:
            return True  # No pattern = no gating
        return self._scan_for_ready_pattern()

    def _log_prompt_check(self, result: bool, reason: str, **extra) -> None:
        """Log prompt check result only on state change (debounce)."""
        if result == self._last_prompt_check_result and reason == self._last_prompt_check_reason:
            return  # Same state, don't log
        self._last_prompt_check_result = result
        self._last_prompt_check_reason = reason
        from ..core.log import log_info

        log_info(
            "pty",
            "prompt.check",
            instance=self.instance_name or "",
            result=result,
            reason=reason,
            **extra,
        )

    def _get_claude_input_text(self) -> tuple[str | None, str]:
        """Extract Claude input box text and reason.

        Returns:
            (text, reason)
            - text: "" if placeholder/empty, user text if present, None if prompt not found
            - reason: "empty", "placeholder", "llm_suggestion", "has_text",
                      "no_prompt_found", or "display_error"
        """
        try:
            display = self._screen.display
        except (IndexError, KeyError, RuntimeError):
            return None, "display_error"

        for row_idx, row in enumerate(display):
            # Find ❯ at start of line
            col = 0
            while col < len(row) and row[col] == " ":
                col += 1
            if col >= len(row) or row[col] != "❯":
                continue

            # Check borders above and below (always present in Claude's UI)
            if row_idx == 0 or "─" not in display[row_idx - 1]:
                continue  # Skip rows without border above
            if row_idx + 1 >= len(display) or "─" not in display[row_idx + 1]:
                continue  # Skip rows without border below

            # Find input area start (after ❯ and space - there's a nbsp \xa0 after ❯)
            input_start_col = col + 1
            while input_start_col < len(row) and row[input_start_col] in " \xa0":
                input_start_col += 1

            text_after_prompt = row[input_start_col:].rstrip()
            if not text_after_prompt:
                return "", "empty"
            if text_after_prompt.startswith('Try "'):
                return "", "placeholder"
            if "↵" in text_after_prompt:
                return "", "llm_suggestion"
            return text_after_prompt, "has_text"

        return None, "no_prompt_found"

    def get_input_box_text(self) -> str | None:
        """Extract text currently in input box.

        Returns:
            str: Text in input box (empty string if placeholder visible)
            None: Input box not found (in menu, not at prompt)

        Note:
            Requires self.tool to be set to a supported tool. If unset, this logs
            and returns None to avoid unreliable heuristics.
        """
        tool = (self.tool or "").lower()
        if tool == "claude":
            text, _reason = self._get_claude_input_text()
            return text
        if tool == "gemini":
            return self._get_gemini_input_text()
        if tool == "codex":
            return self._get_codex_input_text()

        from ..core.log import log_error

        log_error(
            "pty",
            "prompt.detect",
            "tool not set; refusing prompt detection",
            instance=self.instance_name or "",
        )
        return None

    def _get_gemini_input_text(self) -> str | None:
        """Extract Gemini input text or None if prompt not found."""
        if self.is_ready():
            return ""
        try:
            display = self._screen.display
        except (IndexError, KeyError, RuntimeError):
            return None

        for row_idx in range(len(display) - 1):
            if "╭" in display[row_idx] and "│ >" in display[row_idx + 1]:
                row = display[row_idx + 1]
                if "│ >" in row and "│" in row:
                    text = row.split("│ >", 1)[1].split("│", 1)[0].strip()
                    return text
        return None

    def _get_codex_input_text(self) -> str | None:
        """Extract Codex input text or None if prompt not found."""
        if self.is_ready():
            return ""
        try:
            display = self._screen.display
        except (IndexError, KeyError, RuntimeError):
            return None

        for row in display[-10:]:
            stripped = row.lstrip()
            if stripped.startswith("› "):
                return stripped.split("› ", 1)[1].rstrip()
        return None

    def is_prompt_empty(self) -> bool:
        """Check if Claude's input box is visible and has no user text.

        Detection based on Claude Code v2.1.6 source analysis:
        - Static placeholder: always starts with 'Try "' (e.g., 'Try "fix lint errors"')
        - LLM suggestion: always ends with '↵ send' (right-aligned, dimmed)
        - User input: has neither pattern

        LIMITATION: pyte terminal emulator doesn't reliably detect "dim" text attribute.
        Ideally we'd check if text is dimmed (placeholder styling), but pyte only tracks
        fg/bg colors, not intensity. So we use text patterns as heuristics. This means
        if a user types exactly 'Try "...' it could be misclassified as placeholder.
        In practice this is rare and mitigated by user_activity_cooldown blocking
        injection within 0.5s of any keystroke.

        The pattern checks are position-aware: we first locate the ❯ prompt character,
        then check what appears in the input area immediately after it.

        Returns:
            True if prompt visible with placeholder/empty (safe to inject).
            False if user has typed uncommitted text (block injection).
        """
        text, reason = self._get_claude_input_text()
        if text is None:
            self._log_prompt_check(False, reason)
            return False
        if text == "":
            self._log_prompt_check(True, reason)
            return True
        self._log_prompt_check(False, "has_text", text=text[:40])
        return False

    def get_screen_columns(self) -> int:
        """Return current screen column count."""
        return self._screen.columns

    def is_waiting_approval(self) -> bool:
        """Check if tool is waiting for user approval (detected via OSC9 notification).

        Returns:
            True if an OSC9 approval notification was detected and user hasn't responded yet.
        """
        return self._waiting_approval

    def is_output_stable(self, seconds: float = 1.0) -> bool:
        """Check if screen output has been stable.

        Uses pyte's dirty tracking: stable when no screen changes for N seconds.
        Much faster than hashing and catches color-only changes (shimmer).

        Args:
            seconds: Required stability duration in seconds.
        """
        return self._stability_tracker.is_stable(seconds, self.instance_name or "")

    def is_output_stable_simple(self, seconds: float) -> bool:
        """Fast stability check - assumes output bytes drive all changes.

        Uses only _last_output_time, no dirty tracking.
        Faster than is_output_stable() but ignores color-only changes.
        """
        return (time.monotonic() - self._last_output_time) >= seconds

    def wait_for_ready(self, pattern: bytes | None = None, timeout: float = 30.0) -> bool:
        """Wait for child to be ready (output contains pattern).

        If pattern is None, uses:
        - self.ready_pattern (if set), else
        - a generic fallback (b'shortcuts')
        """
        pattern = pattern or self.ready_pattern or b"shortcuts"
        buffer = b""
        start = time.time()
        stdin_fd = sys.stdin.fileno()

        if self._debug_enabled:
            self._debug_log(f"wait_for_ready: pattern={pattern!r}, timeout={timeout}")
            self._debug_last_periodic_dump = time.time()

        while time.time() - start < timeout:
            # Periodic screen dump every 5s when debug enabled
            if self._debug_enabled and time.time() - self._debug_last_periodic_dump >= 5.0:
                self._debug_dump_screen(f"Periodic dump (elapsed: {time.time() - start:.1f}s)")
                self._debug_last_periodic_dump = time.time()

            if not self._ptyproc or not self._ptyproc.isalive():
                if self._debug_enabled:
                    self._debug_log("wait_for_ready: process died")
                return False

            try:
                # Also listen on stdin to forward terminal responses (e.g., cursor position)
                rlist, _, _ = select.select([self._ptyproc.fd, sys.stdin], [], [], 0.5)

                # Forward stdin to child (terminal responses like cursor position)
                if sys.stdin in rlist:
                    try:
                        stdin_data = os.read(stdin_fd, 1024)
                        if stdin_data and self._ptyproc:
                            os.write(self._ptyproc.fd, stdin_data)
                    except OSError:
                        pass

                # Read child output
                if self._ptyproc.fd in rlist:
                    data = os.read(self._ptyproc.fd, 1024)
                    if not data:
                        if self._debug_enabled:
                            self._debug_log("wait_for_ready: EOF from child")
                        return False
                    buffer += data
                    self._update_output_buffer(data)
                    if self.on_output:
                        try:
                            self.on_output(data)
                        except Exception:
                            pass  # Don't break ready loop on callback errors
                    sys.stdout.buffer.write(data)
                    sys.stdout.flush()

                    if self._debug_enabled and pattern in data:
                        self._debug_log("wait_for_ready: PATTERN FOUND IN DATA!")
                        self._debug_dump_screen("Pattern found in output")

                    if pattern in buffer:
                        if self._debug_enabled:
                            self._debug_log("wait_for_ready: Pattern found in buffer, invoking on_ready")
                            self._debug_dump_screen("Before invoking on_ready")
                        self._invoke_on_ready()
                        return True
            except OSError as e:
                if self._debug_enabled:
                    self._debug_log(f"wait_for_ready: OSError {e}")
                return False

        # Timeout, continue anyway
        if self._debug_enabled:
            self._debug_log(f"wait_for_ready: Timeout after {timeout}s, invoking on_ready anyway")
            self._debug_dump_screen("Timeout in wait_for_ready")
        self._invoke_on_ready()
        return True

    def _debug_log(self, msg: str) -> None:
        """Log debug message to file."""
        if self._debug_enabled and self._debug_file:
            try:
                self._debug_file.write(f"{msg}\n")
                self._debug_file.flush()
            except Exception:
                pass

    def _invoke_on_ready(self) -> None:
        """Invoke on_ready callback if set and port is available."""
        if self._debug_enabled:
            self._debug_log(
                f"_invoke_on_ready called: callback={self._on_ready_callback is not None}, port={self.actual_port}"
            )
        if self._on_ready_callback and self.actual_port:
            try:
                if self._debug_enabled:
                    self._debug_log(f"Calling on_ready callback with port {self.actual_port}")
                self._on_ready_callback(self.actual_port)
                if self._debug_enabled:
                    self._debug_log("on_ready callback completed successfully")
            except Exception as e:
                if self._debug_enabled:
                    self._debug_log(f"on_ready callback raised: {e}")
                pass  # Don't break on callback errors

    def _debug_dump_screen(self, label: str = "") -> None:
        """Dump screen state to debug log (when HCOM_PTY_DEBUG=1)."""
        if not self._debug_enabled or not self._screen:
            return

        self._debug_counter += 1
        self._debug_log(f"\n=== SCREEN DUMP {self._debug_counter}: {label} ===")
        self._debug_log(f"Instance: {self.instance_name}")
        self._debug_log(f"Ready pattern: {self.ready_pattern!r}")
        self._debug_log(f"Actual port: {self.actual_port}")
        self._debug_log(f"Callback set: {self._on_ready_callback is not None}")
        self._debug_log(f"Screen size: {self._screen.lines}x{self._screen.columns}")
        self._debug_log(f"Cursor: ({self._screen.cursor.y}, {self._screen.cursor.x})")

        # Show non-empty screen lines
        self._debug_log("Screen content (non-empty lines):")
        for i, line in enumerate(self._screen.display):
            if line.strip():
                self._debug_log(f"  {i:3d}: {line}")

        # Check for ready pattern in screen
        if self.ready_pattern:
            pattern_str = self.ready_pattern.decode("utf-8", errors="replace")
            found = any(pattern_str in line for line in self._screen.display)
            self._debug_log(f"Ready pattern '{pattern_str}' in screen: {found}")

        self._debug_log(f"is_ready(): {self.is_ready()}")
        self._debug_log("")

    def inject(self, text: str, submit: bool = True) -> bool:
        """Inject text to the PTY (called from same process).

        Args:
            text: Text to inject
            submit: Whether to send Enter after text (default True)
        """
        if not self._ptyproc or not self._ptyproc.isalive():
            return False
        try:
            if text.endswith("\n"):
                text = text[:-1]
            if text:
                os.write(self._ptyproc.fd, text.encode("utf-8", "surrogateescape"))
                time.sleep(0.1)  # Wait for terminal to process before Enter
            if submit:
                os.write(self._ptyproc.fd, b"\r")  # Send Enter (CR, same as keyboard)
            return True
        except OSError:
            return False

    def run(self) -> int:
        """Run the PTY wrapper. Returns exit code."""
        # Set terminal title
        self.set_terminal_title()

        # Setup signal handlers
        signal.signal(signal.SIGWINCH, self.handle_sigwinch)
        signal.signal(signal.SIGTERM, self.handle_signal)
        signal.signal(signal.SIGINT, self.handle_signal)

        # Spawn child process with ptyprocess
        rows, cols = self.get_terminal_size()
        self._ptyproc = ptyprocess.PtyProcess.spawn(
            self.command,
            dimensions=(rows, cols),
        )
        self.pid = self._ptyproc.pid
        self.running = True

        # Store PID in DB for hcom stop/kill
        if self.instance_name and self.pid:
            try:
                from ..core.instances import update_instance_position

                update_instance_position(self.instance_name, {"pid": self.pid})
            except Exception:
                pass

        # Create wake pipe for shutdown
        self._wake_r, self._wake_w = os.pipe()
        os.set_blocking(self._wake_r, False)

        # Create selector
        self._sel = selectors.DefaultSelector()

        # Set up injection server (non-blocking)
        self._setup_injection_server()

        # Save terminal settings and set raw mode
        if os.isatty(sys.stdin.fileno()):
            self.orig_termios = termios.tcgetattr(sys.stdin)

        try:
            if self.orig_termios:
                tty.setraw(sys.stdin)

            # Wait for child to be ready
            self.wait_for_ready()

            # Main I/O loop
            self.io_loop()

        finally:
            self.cleanup()

        # Get exit code
        exit_code = 0
        if self._ptyproc:
            try:
                self._ptyproc.wait()
                exit_code = self._ptyproc.exitstatus or 0
            except Exception:
                pass

        return exit_code

    def cleanup(self) -> None:
        """Cleanup resources."""
        self.running = False

        # Cancel pending resize timer
        if self._resize_timer is not None:
            self._resize_timer.cancel()
            self._resize_timer = None

        # Terminate child process
        if self._ptyproc and self._ptyproc.isalive():
            try:
                self._ptyproc.terminate(force=True)
            except Exception:
                pass

        # Restore default signal handlers so Ctrl+C works normally after exit
        signal.signal(signal.SIGWINCH, signal.SIG_DFL)
        signal.signal(signal.SIGTERM, signal.SIG_DFL)
        signal.signal(signal.SIGINT, signal.SIG_DFL)

        # Restore terminal
        if self.orig_termios:
            try:
                termios.tcsetattr(sys.stdin, termios.TCSADRAIN, self.orig_termios)
            except Exception:
                pass
            self.orig_termios = None

        # Close server socket
        if self.server_socket:
            try:
                self.server_socket.close()
            except Exception:
                pass
            self.server_socket = None

        # Close any pending injection clients
        for conn in list(self._inject_clients.keys()):
            try:
                conn.close()
            except Exception:
                pass
        self._inject_clients.clear()

        # Close selector
        if self._sel:
            try:
                self._sel.close()
            except Exception:
                pass
            self._sel = None

        # Close wake pipe
        if self._wake_r >= 0:
            try:
                os.close(self._wake_r)
            except OSError:
                pass
            self._wake_r = -1
        if self._wake_w >= 0:
            try:
                os.close(self._wake_w)
            except OSError:
                pass
            self._wake_w = -1


__all__ = ["PTYWrapper", "ScreenStabilityTracker"]
