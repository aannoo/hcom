#!/usr/bin/env python3
"""Tests for strict tool-args validation override env var."""

from __future__ import annotations

import pytest

from hcom.commands.utils import CLIError


def test_claude_cmd_launch_strict_blocks_unknown_flag(monkeypatch, capsys):
    from hcom.commands.lifecycle import cmd_launch_tool

    monkeypatch.delenv("HCOM_SKIP_TOOL_ARGS_VALIDATION", raising=False)

    with pytest.raises(CLIError) as excinfo:
        cmd_launch_tool("claude", ["--moddel", "haiku"])
    assert "unknown option" in str(excinfo.value).lower()


def test_claude_cmd_launch_override_allows_unknown_flag(monkeypatch):
    from hcom.commands.lifecycle import cmd_launch_tool

    monkeypatch.setenv("HCOM_SKIP_TOOL_ARGS_VALIDATION", "1")

    def fake_launch(*args, **kwargs):
        return {"batch_id": "test", "launched": 1, "failed": 0, "background": False, "log_files": [], "handles": [], "errors": []}

    monkeypatch.setattr("hcom.launcher.launch", fake_launch)

    rc = cmd_launch_tool("claude", ["--moddel", "haiku"])
    assert rc == 0


def test_gemini_cmd_launch_strict_blocks_unknown_flag(monkeypatch):
    from hcom.commands.lifecycle import cmd_launch_tool

    monkeypatch.delenv("HCOM_SKIP_TOOL_ARGS_VALIDATION", raising=False)

    with pytest.raises(CLIError) as excinfo:
        cmd_launch_tool("gemini", ["1", "gemini", "--moddel"])

    assert "unknown option" in str(excinfo.value).lower()


def test_gemini_cmd_launch_override_allows_unknown_flag(monkeypatch):
    from hcom.commands.lifecycle import cmd_launch_tool

    monkeypatch.setenv("HCOM_SKIP_TOOL_ARGS_VALIDATION", "1")

    def fake_launch(*args, **kwargs):
        return {"batch_id": "test", "launched": 1, "failed": 0, "handles": [{"instance_name": "g1"}], "errors": []}

    monkeypatch.setattr("hcom.launcher.launch", fake_launch)

    rc = cmd_launch_tool("gemini", ["1", "gemini", "--moddel"])
    assert rc == 0


def test_codex_cmd_launch_strict_blocks_unknown_flag(monkeypatch):
    from hcom.commands.lifecycle import cmd_launch_tool

    monkeypatch.delenv("HCOM_SKIP_TOOL_ARGS_VALIDATION", raising=False)

    with pytest.raises(CLIError) as excinfo:
        cmd_launch_tool("codex", ["1", "codex", "--moddel"])

    assert "unknown option" in str(excinfo.value).lower()


def test_codex_cmd_launch_override_allows_unknown_flag(monkeypatch):
    from hcom.commands.lifecycle import cmd_launch_tool

    monkeypatch.setenv("HCOM_SKIP_TOOL_ARGS_VALIDATION", "1")

    def fake_launch(*args, **kwargs):
        return {"batch_id": "test", "launched": 1, "failed": 0, "handles": [{"instance_name": "c1"}], "errors": []}

    monkeypatch.setattr("hcom.launcher.launch", fake_launch)

    rc = cmd_launch_tool("codex", ["1", "codex", "--moddel"])
    assert rc == 0
