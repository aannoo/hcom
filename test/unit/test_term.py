"""Unit tests for hcom term command and terminal operations.

Tests screen query, text injection, debug toggle, formatting,
kill_process, close_terminal_pane, resolve_terminal_info, and pidtrack terminal fields.
"""

import json
import os
import signal
import socket
import threading
import time
from unittest.mock import MagicMock, patch

import pytest

from hcom.commands.term import (
    cmd_term,
    _format_screen,
    _get_inject_port,
    _get_pty_instances,
    _inject_raw,
    _inject_text,
    _query_screen,
)
from hcom.terminal import (
    KillResult,
    TerminalInfo,
    close_terminal_pane,
    detect_terminal_from_env,
    kill_process,
    resolve_terminal_info,
)


# ==================== _format_screen ====================

class TestFormatScreen:
    def test_basic_format(self):
        data = {
            "lines": ["hello", "", "world"],
            "cursor": [2, 5],
            "size": [24, 80],
            "ready": True,
            "prompt_empty": False,
            "input_text": "foo",
        }
        out = _format_screen(data)
        assert "Screen 24x80" in out
        assert "cursor (2,5)" in out
        assert "ready=True" in out
        assert "prompt_empty=False" in out
        assert "'foo'" in out
        assert "  0: hello" in out
        assert "  2: world" in out
        # Empty line (index 1) should not appear
        assert "  1:" not in out

    def test_empty_screen(self):
        data = {"lines": [], "cursor": [0, 0], "size": [0, 0]}
        out = _format_screen(data)
        assert "Screen 0x0" in out

    def test_missing_fields_use_defaults(self):
        out = _format_screen({})
        assert "Screen 0x0" in out
        assert "cursor (0,0)" in out


# ==================== _flag_path / debug ====================

class TestDebug:
    def test_debug_on_creates_flag(self, tmp_path, monkeypatch):
        monkeypatch.setattr("hcom.commands.term.hcom_path", lambda *parts: tmp_path.joinpath(*parts))
        assert cmd_term(["debug", "on"]) == 0
        assert (tmp_path / ".tmp" / "pty_debug_on").exists()

    def test_debug_off_removes_flag(self, tmp_path, monkeypatch):
        flag = tmp_path / ".tmp" / "pty_debug_on"
        flag.parent.mkdir(parents=True)
        flag.touch()
        monkeypatch.setattr("hcom.commands.term.hcom_path", lambda *parts: tmp_path.joinpath(*parts))
        assert cmd_term(["debug", "off"]) == 0
        assert not flag.exists()

    def test_debug_off_no_flag_ok(self, tmp_path, monkeypatch):
        monkeypatch.setattr("hcom.commands.term.hcom_path", lambda *parts: tmp_path.joinpath(*parts))
        assert cmd_term(["debug", "off"]) == 0

    def test_debug_no_subcommand(self):
        assert cmd_term(["debug"]) == 0

    def test_debug_logs(self, tmp_path, monkeypatch):
        log_dir = tmp_path / ".tmp" / "logs" / "pty_debug"
        log_dir.mkdir(parents=True)
        (log_dir / "test.log").write_text("data")
        monkeypatch.setattr("hcom.commands.term.hcom_path", lambda *parts: tmp_path.joinpath(*parts))
        assert cmd_term(["debug", "logs"]) == 0

    def test_debug_logs_no_dir(self, tmp_path, monkeypatch):
        monkeypatch.setattr("hcom.commands.term.hcom_path", lambda *parts: tmp_path.joinpath(*parts))
        assert cmd_term(["debug", "logs"]) == 0


# ==================== _query_screen ====================

class TestQueryScreen:
    def _make_server(self, response: bytes):
        """Start a TCP server that responds to first connection."""
        srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind(("127.0.0.1", 0))
        srv.listen(1)
        port = srv.getsockname()[1]

        def handler():
            conn, _ = srv.accept()
            conn.recv(1024)  # read query
            conn.sendall(response)
            conn.close()
            srv.close()

        t = threading.Thread(target=handler, daemon=True)
        t.start()
        return port

    def test_valid_json_response(self):
        data = {"lines": ["hi"], "size": [24, 80], "cursor": [0, 2], "ready": True, "prompt_empty": True, "input_text": None}
        port = self._make_server(json.dumps(data).encode())
        result = _query_screen(port)
        assert result is not None
        assert result["lines"] == ["hi"]
        assert result["size"] == [24, 80]
        assert result["ready"] is True

    def test_dead_port(self):
        # Use a port nothing is listening on
        result = _query_screen(1, timeout=0.1)
        assert result is None

    def test_invalid_json(self):
        port = self._make_server(b"not json")
        result = _query_screen(port)
        assert result is None

    def test_empty_response(self):
        port = self._make_server(b"")
        result = _query_screen(port)
        assert result is None


# ==================== inject ====================

class TestInject:
    def _make_sink(self):
        """TCP server that captures received data."""
        srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        srv.bind(("127.0.0.1", 0))
        srv.listen(5)
        port = srv.getsockname()[1]
        received = []

        def handler():
            while True:
                try:
                    conn, _ = srv.accept()
                    data = conn.recv(4096)
                    received.append(data)
                    conn.close()
                except OSError:
                    break

        t = threading.Thread(target=handler, daemon=True)
        t.start()
        return port, received, srv

    def test_inject_raw(self):
        port, received, srv = self._make_sink()
        _inject_raw(port, b"hello")
        time.sleep(0.05)
        srv.close()
        assert received == [b"hello"]

    def test_inject_text_only(self):
        port, received, srv = self._make_sink()
        with patch("hcom.commands.term._get_inject_port", return_value=port):
            rc = _inject_text("test", "hello")
        time.sleep(0.05)
        srv.close()
        assert rc == 0
        assert b"hello" in received

    def test_inject_enter_only(self):
        port, received, srv = self._make_sink()
        with patch("hcom.commands.term._get_inject_port", return_value=port):
            rc = _inject_text("test", "", enter=True)
        time.sleep(0.05)
        srv.close()
        assert rc == 0
        assert b"\r" in received

    def test_inject_text_and_enter(self):
        port, received, srv = self._make_sink()
        with patch("hcom.commands.term._get_inject_port", return_value=port):
            rc = _inject_text("test", "hi", enter=True)
        time.sleep(0.2)  # 100ms delay between connections
        srv.close()
        assert rc == 0
        assert len(received) == 2
        assert received[0] == b"hi"
        assert received[1] == b"\r"

    def test_inject_no_port(self):
        with patch("hcom.commands.term._get_inject_port", return_value=None):
            rc = _inject_text("test", "hi")
        assert rc == 1

    def test_inject_connection_refused(self):
        with patch("hcom.commands.term._get_inject_port", return_value=1):
            rc = _inject_text("test", "hi")
        assert rc == 1


# ==================== cmd_term dispatch ====================

class TestCmdTerm:
    def test_help(self):
        assert cmd_term(["--help"]) == 0

    def test_inject_missing_name(self):
        assert cmd_term(["inject"]) == 1

    def test_inject_nothing_to_inject(self):
        assert cmd_term(["inject", "myname"]) == 1

    def test_screen_no_port(self):
        with patch("hcom.commands.term._get_inject_port", return_value=None):
            assert cmd_term(["myname"]) == 1

    def test_screen_no_instances(self):
        with patch("hcom.commands.term._get_pty_instances", return_value=[]):
            assert cmd_term([]) == 1

    def test_screen_json_flag(self):
        data = {"lines": [], "size": [24, 80], "cursor": [0, 0], "ready": True, "prompt_empty": True, "input_text": None}
        with patch("hcom.commands.term._get_inject_port", return_value=9999), \
             patch("hcom.commands.term._query_screen", return_value=data):
            assert cmd_term(["myname", "--json"]) == 0

    def test_screen_no_response(self):
        with patch("hcom.commands.term._get_inject_port", return_value=9999), \
             patch("hcom.commands.term._query_screen", return_value=None):
            assert cmd_term(["myname"]) == 1

    def test_screen_all_instances(self):
        data = {"lines": ["hi"], "size": [24, 80], "cursor": [0, 0], "ready": True, "prompt_empty": True, "input_text": None}
        with patch("hcom.commands.term._get_pty_instances", return_value=[{"name": "test", "port": 9999}]), \
             patch("hcom.commands.term._query_screen", return_value=data):
            assert cmd_term([]) == 0

    def test_screen_all_none_responding(self):
        with patch("hcom.commands.term._get_pty_instances", return_value=[{"name": "test", "port": 9999}]), \
             patch("hcom.commands.term._query_screen", return_value=None):
            assert cmd_term([]) == 1


# ==================== DB integration ====================

class TestDbLookup:
    def test_get_inject_port_no_db(self):
        """Graceful None when DB unavailable."""
        with patch("hcom.core.db.get_db", side_effect=Exception("no db")):
            assert _get_inject_port("test") is None

    def test_get_pty_instances_no_db(self):
        with patch("hcom.core.db.get_db", side_effect=Exception("no db")):
            assert _get_pty_instances() == []

    def test_get_inject_port_found(self, hcom_env):
        from hcom.core.db import init_db, get_db
        init_db()
        get_db().execute(
            "INSERT INTO notify_endpoints (instance, kind, port, updated_at) VALUES (?, 'inject', ?, ?)",
            ("test", 12345, 0.0),
        )
        assert _get_inject_port("test") == 12345

    def test_get_inject_port_not_found(self, hcom_env):
        from hcom.core.db import init_db
        init_db()
        assert _get_inject_port("nonexistent") is None

    def test_get_pty_instances(self, hcom_env):
        from hcom.core.db import init_db, get_db
        init_db()
        get_db().execute(
            "INSERT INTO notify_endpoints (instance, kind, port, updated_at) VALUES (?, 'inject', ?, ?)",
            ("a", 111, 0.0),
        )
        get_db().execute(
            "INSERT INTO notify_endpoints (instance, kind, port, updated_at) VALUES (?, 'inject', ?, ?)",
            ("b", 222, 0.0),
        )
        result = _get_pty_instances()
        assert len(result) == 2
        names = {r["name"] for r in result}
        assert names == {"a", "b"}


# ==================== KillResult enum ====================

class TestKillResult:
    def test_enum_values(self):
        assert KillResult.SENT.value == "sent"
        assert KillResult.ALREADY_DEAD.value == "already_dead"
        assert KillResult.PERMISSION_DENIED.value == "permission_denied"

    def test_all_truthy(self):
        """All enum members are truthy (no accidental falsy comparisons)."""
        for member in KillResult:
            assert member


# ==================== kill_process ====================

class TestKillProcess:
    def test_sent(self):
        with patch("os.killpg") as mock_kill:
            result, pane_closed = kill_process(12345)
            assert result == KillResult.SENT
            assert pane_closed is False
            mock_kill.assert_called_once_with(12345, signal.SIGTERM)

    def test_already_dead(self):
        with patch("os.killpg", side_effect=ProcessLookupError):
            result, _ = kill_process(12345)
            assert result == KillResult.ALREADY_DEAD

    def test_permission_denied(self):
        with patch("os.killpg", side_effect=PermissionError):
            result, _ = kill_process(12345)
            assert result == KillResult.PERMISSION_DENIED

    def test_closes_pane_before_sigterm(self):
        """close_terminal_pane runs before SIGTERM."""
        call_order = []
        with (
            patch("hcom.terminal.close_terminal_pane", side_effect=lambda *a, **k: call_order.append("close") or True) as mock_ct,
            patch("os.killpg", side_effect=lambda *a: call_order.append("kill")),
        ):
            result, pane_closed = kill_process(123, preset_name="wezterm", pane_id="42")
            assert result == KillResult.SENT
            assert pane_closed is True
            assert call_order == ["close", "kill"]
            mock_ct.assert_called_once_with(123, "wezterm", pane_id="42", process_id="", kitty_listen_on="", terminal_id="")

    def test_no_close_without_preset(self):
        with (
            patch("hcom.terminal.close_terminal_pane") as mock_ct,
            patch("os.killpg"),
        ):
            kill_process(123)
            mock_ct.assert_not_called()

    def test_sigterm_sent_even_if_close_fails(self):
        with (
            patch("hcom.terminal.close_terminal_pane", return_value=False),
            patch("os.killpg") as mock_kill,
        ):
            result, _ = kill_process(123, preset_name="wezterm", pane_id="42")
            assert result == KillResult.SENT
            mock_kill.assert_called_once()


# ==================== close_terminal_pane ====================

class TestCloseTerminalPane:
    def test_no_preset(self):
        with patch("hcom.core.settings.get_merged_preset", return_value=None):
            assert close_terminal_pane(123, "nonexistent") is False

    def test_no_close_command(self):
        with patch("hcom.core.settings.get_merged_preset", return_value={"open": "x {script}", "close": None}):
            assert close_terminal_pane(123, "alacritty") is False

    def test_missing_pane_id(self):
        preset = {"open": "x {script}", "close": "kill-pane --id {pane_id}"}
        with patch("hcom.core.settings.get_merged_preset", return_value=preset):
            assert close_terminal_pane(123, "wezterm", pane_id="") is False

    def test_missing_process_id(self):
        preset = {"open": "x {script}", "close": "close --match env:ID={process_id}"}
        with patch("hcom.core.settings.get_merged_preset", return_value=preset):
            assert close_terminal_pane(123, "kitty", process_id="") is False

    def test_successful_close(self):
        preset = {"open": "x {script}", "close": "wezterm cli kill-pane --pane-id {pane_id}", "binary": "wezterm"}
        mock_result = MagicMock(returncode=0)
        with (
            patch("hcom.core.settings.get_merged_preset", return_value=preset),
            patch("shutil.which", return_value="/usr/bin/wezterm"),
            patch("subprocess.run", return_value=mock_result) as mock_run,
        ):
            assert close_terminal_pane(123, "wezterm", pane_id="42") is True
            cmd = mock_run.call_args[0][0]
            assert "42" in cmd
            assert "{pane_id}" not in cmd

    def test_failed_close(self):
        preset = {"open": "x {script}", "close": "wezterm cli kill-pane --pane-id {pane_id}", "binary": "wezterm"}
        mock_result = MagicMock(returncode=1)
        with (
            patch("hcom.core.settings.get_merged_preset", return_value=preset),
            patch("shutil.which", return_value="/usr/bin/wezterm"),
            patch("subprocess.run", return_value=mock_result),
        ):
            assert close_terminal_pane(123, "wezterm", pane_id="42") is False

    def test_timeout(self):
        import subprocess as sp
        preset = {"open": "x {script}", "close": "slow-cmd {pane_id}", "binary": "slow-cmd"}
        with (
            patch("hcom.core.settings.get_merged_preset", return_value=preset),
            patch("shutil.which", return_value="/usr/bin/slow-cmd"),
            patch("subprocess.run", side_effect=sp.TimeoutExpired("cmd", 5)),
        ):
            assert close_terminal_pane(123, "test", pane_id="1") is False

    def test_process_id_substitution(self):
        """Kitty-style close with {process_id}."""
        preset = {"open": "x {script}", "close": "kitten @ close-window --match env:HCOM_PROCESS_ID={process_id}"}
        mock_result = MagicMock(returncode=0)
        with (
            patch("hcom.core.settings.get_merged_preset", return_value=preset),
            patch("subprocess.run", return_value=mock_result) as mock_run,
        ):
            assert close_terminal_pane(123, "kitty", process_id="uuid-abc") is True
            cmd = mock_run.call_args[0][0]
            assert "uuid-abc" in cmd
            assert "{process_id}" not in cmd


# ==================== detect_terminal_from_env ====================

class TestDetectTerminalFromEnv:
    def test_tmux(self):
        with patch.dict(os.environ, {"TMUX_PANE": "%3"}, clear=False):
            assert detect_terminal_from_env() == "tmux-split"

    def test_wezterm(self):
        with patch.dict(os.environ, {"WEZTERM_PANE": "5"}, clear=False):
            assert detect_terminal_from_env() == "wezterm-split"

    def test_kitty(self):
        with patch.dict(os.environ, {"KITTY_WINDOW_ID": "123"}, clear=False):
            assert detect_terminal_from_env() == "kitty-split"

    def test_none(self):
        env = {k: v for k, v in os.environ.items()
               if k not in ("TMUX_PANE", "WEZTERM_PANE", "KITTY_WINDOW_ID", "ZELLIJ_PANE_ID", "WAVETERM_BLOCKID")}
        with patch.dict(os.environ, env, clear=True):
            assert detect_terminal_from_env() is None


# ==================== resolve_terminal_info ====================

class TestResolveTerminalInfo:
    def test_from_launch_context(self):
        """Primary path: data from DB launch_context."""
        lc = json.dumps({"terminal_preset": "wezterm", "pane_id": "42", "process_id": "uuid-1"})
        pos = {"launch_context": lc}
        with patch("hcom.core.instances.load_instance_position", return_value=pos):
            info = resolve_terminal_info("test", 123)
            assert info.preset_name == "wezterm"
            assert info.pane_id == "42"
            assert info.process_id == "uuid-1"

    def test_fallback_to_pidtrack(self):
        """When launch_context empty, falls back to pidtrack."""
        with (
            patch("hcom.core.instances.load_instance_position", return_value=None),
            patch("hcom.core.pidtrack.get_preset_for_pid", return_value="tmux-split"),
            patch("hcom.core.pidtrack.get_pane_id_for_pid", return_value="%2"),
            patch("hcom.core.db.get_db") as mock_db,
        ):
            mock_db.return_value.execute.return_value.fetchone.return_value = {"process_id": "uuid-2"}
            info = resolve_terminal_info("test", 456)
            assert info.preset_name == "tmux-split"
            assert info.pane_id == "%2"
            assert info.process_id == "uuid-2"

    def test_process_id_from_launch_context_skips_db(self):
        """When process_id is in launch_context, no DB query needed."""
        lc = json.dumps({"terminal_preset": "kitty", "pane_id": "", "process_id": "uuid-3"})
        pos = {"launch_context": lc}
        with (
            patch("hcom.core.instances.load_instance_position", return_value=pos),
            patch("hcom.core.db.get_db") as mock_db,
        ):
            info = resolve_terminal_info("test", 789)
            assert info.process_id == "uuid-3"
            mock_db.assert_not_called()

    def test_all_empty(self):
        """Graceful when nothing found."""
        with (
            patch("hcom.core.instances.load_instance_position", return_value=None),
            patch("hcom.core.pidtrack.get_preset_for_pid", return_value=None),
            patch("hcom.core.pidtrack.get_pane_id_for_pid", return_value=""),
            patch("hcom.core.db.get_db") as mock_db,
        ):
            mock_db.return_value.execute.return_value.fetchone.return_value = None
            info = resolve_terminal_info("test", 0)
            assert info.preset_name == ""
            assert info.pane_id == ""
            assert info.process_id == ""


# ==================== pidtrack terminal fields ====================

class TestPidtrackTerminalFields:
    """Test that pidtrack correctly stores and retrieves terminal info."""

    @pytest.fixture(autouse=True)
    def setup_pidtrack(self, tmp_path):
        """Redirect pidtrack to temp file."""
        pidfile = tmp_path / "pids.json"
        with patch("hcom.core.pidtrack._pidfile_path", return_value=pidfile):
            from hcom.core.pidtrack import _invalidate_cache
            _invalidate_cache()
            yield
            _invalidate_cache()

    def test_record_and_retrieve_preset(self):
        from hcom.core.pidtrack import get_preset_for_pid, record_pid
        record_pid(100, "claude", "test-inst", terminal_preset="wezterm")
        assert get_preset_for_pid(100) == "wezterm"

    def test_record_and_retrieve_pane_id(self):
        from hcom.core.pidtrack import get_pane_id_for_pid, record_pid
        record_pid(101, "claude", "test-inst", pane_id="42")
        assert get_pane_id_for_pid(101) == "42"

    def test_orphan_includes_terminal_fields(self):
        from hcom.core.pidtrack import get_orphan_processes, record_pid
        record_pid(99999, "claude", "test-inst", terminal_preset="tmux-split", pane_id="%3")
        with patch("os.kill"):
            orphans = get_orphan_processes(active_pids=set())
        assert len(orphans) >= 1
        orphan = next(o for o in orphans if o["pid"] == 99999)
        assert orphan["terminal_preset"] == "tmux-split"
        assert orphan["pane_id"] == "%3"

    def test_no_overwrite_existing_fields(self):
        from hcom.core.pidtrack import get_pane_id_for_pid, get_preset_for_pid, record_pid
        record_pid(103, "claude", "inst-a", terminal_preset="wezterm", pane_id="1")
        record_pid(103, "claude", "inst-b", terminal_preset="tmux", pane_id="2")
        assert get_preset_for_pid(103) == "wezterm"
        assert get_pane_id_for_pid(103) == "1"

    def test_missing_fields_return_defaults(self):
        from hcom.core.pidtrack import get_pane_id_for_pid, get_preset_for_pid, record_pid
        record_pid(104, "claude", "test-inst")
        assert get_preset_for_pid(104) is None
        assert get_pane_id_for_pid(104) == ""


# ==================== Settings merge preserves fields ====================

class TestSettingsMerge:
    def test_builtin_presets_have_pane_id_env(self):
        from hcom.core.settings import get_merged_presets
        merged = get_merged_presets()
        assert merged["wezterm"].get("pane_id_env") == "WEZTERM_PANE"
        assert merged["tmux-split"].get("pane_id_env") == "TMUX_PANE"

    def test_builtin_presets_have_app_name(self):
        from hcom.core.settings import get_merged_presets
        merged = get_merged_presets()
        assert merged["wezterm"].get("app_name") == "WezTerm"
        assert merged["kitty-tab"].get("app_name") == "kitty"

    def test_toml_override_preserves_builtin_fields(self):
        """TOML override keeps pane_id_env and app_name from builtin."""
        from hcom.core.settings import get_merged_presets, invalidate_settings_cache
        # load_settings returns the terminal section; presets are nested under "presets"
        toml_data = {"presets": {"wezterm": {"open": "custom-wezterm start -- bash {script}"}}}
        with patch("hcom.core.settings.load_settings", return_value=toml_data):
            invalidate_settings_cache()
            merged = get_merged_presets()
            wez = merged["wezterm"]
            assert wez["open"] == "custom-wezterm start -- bash {script}"
            assert wez.get("pane_id_env") == "WEZTERM_PANE"
            assert wez.get("app_name") == "WezTerm"
            assert wez.get("close") == "wezterm cli kill-pane --pane-id {pane_id}"


# ==================== Preset casing alias ====================

class TestPresetCasingAlias:
    def test_config_validation_resolves_casing(self):
        from hcom.core.config import HcomConfig
        config = HcomConfig(terminal="WezTerm")
        config.validate()
        assert config.terminal == "wezterm"

    def test_config_validation_resolves_alacritty(self):
        from hcom.core.config import HcomConfig
        config = HcomConfig(terminal="Alacritty")
        config.validate()
        assert config.terminal == "alacritty"

    def test_lowercase_passes_unchanged(self):
        from hcom.core.config import HcomConfig
        config = HcomConfig(terminal="wezterm")
        config.validate()
        assert config.terminal == "wezterm"
