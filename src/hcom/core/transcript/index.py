"""Thin index over transcript files.

Scans file once, stores (line_no, byte_offset, role, timestamp) per entry.
Consumers navigate by position and read raw entries on demand.

OpenCode uses SQLite instead of flat files -- _build_opencode_sqlite queries
messages+parts tables and caches the results for read_raw access.
"""

from __future__ import annotations

import json
import sqlite3
import threading
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path

from .classify import classify_claude, classify_codex, classify_gemini, classify_opencode


def _epoch_ms_to_iso(ms: int) -> str:
    """Convert epoch milliseconds to ISO 8601 string."""
    return datetime.fromtimestamp(ms / 1000, tz=timezone.utc).isoformat()


@dataclass(frozen=True, slots=True)
class IndexEntry:
    line_no: int
    byte_offset: int  # byte position in file (for JSONL) or message index (for JSON)
    role: str
    timestamp: str


class TranscriptIndex:
    """Index of transcript entries with on-demand raw access."""

    _cache: dict[tuple[str, float], TranscriptIndex] = {}
    _cache_lock = threading.Lock()

    def __init__(self, path: str, agent: str, entries: list[IndexEntry]):
        self.path = path
        self.agent = agent
        self._entries = entries
        self._opencode_messages: list[dict] = []

    def __len__(self) -> int:
        return len(self._entries)

    def __getitem__(self, idx):
        return self._entries[idx]

    def __iter__(self):
        return iter(self._entries)

    @classmethod
    def build(cls, path: str, agent: str, session_id: str | None = None) -> TranscriptIndex:
        """Build index from transcript file. Cached by (path, mtime).

        For opencode, reads from SQLite DB and caches messages+parts.
        """
        p = Path(path)
        if not p.exists():
            return cls(path, agent, [])

        if agent == "opencode":
            # SQLite: skip mtime cache (DB changes constantly, queries are fast)
            entries, messages = cls._build_opencode_sqlite(path, session_id or "")
            index = cls(path, agent, entries)
            index._opencode_messages = messages
            return index

        mtime = p.stat().st_mtime
        cache_key = (str(path), mtime)
        with cls._cache_lock:
            if cache_key in cls._cache:
                return cls._cache[cache_key]

        if agent == "gemini":
            entries = cls._build_gemini(path)
        else:
            classifier = classify_claude if agent == "claude" else classify_codex
            entries = cls._build_jsonl(path, classifier)

        index = cls(path, agent, entries)
        with cls._cache_lock:
            # Evict stale entry for same path with different mtime
            path_str = str(path)
            stale = [k for k in cls._cache if k[0] == path_str and k[1] != mtime]
            for k in stale:
                del cls._cache[k]
            cls._cache[cache_key] = index
        return index

    @staticmethod
    def _build_jsonl(path: str, classifier) -> list[IndexEntry]:
        """Build index from JSONL file (Claude, Codex)."""
        entries = []
        with open(path, "rb") as f:
            byte_offset = 0
            line_no = 0
            for raw_line in f:
                line = raw_line.decode("utf-8", errors="replace")
                stripped = line.strip()
                if not stripped:
                    byte_offset += len(raw_line)
                    line_no += 1
                    continue
                try:
                    obj = json.loads(stripped)
                    role = classifier(obj)
                    timestamp = obj.get("timestamp", "")
                    entries.append(IndexEntry(line_no, byte_offset, role, timestamp))
                except json.JSONDecodeError:
                    entries.append(IndexEntry(line_no, byte_offset, "unknown", ""))
                byte_offset += len(raw_line)
                line_no += 1
        return entries

    @staticmethod
    def _build_gemini(path: str) -> list[IndexEntry]:
        """Build index from Gemini JSON file."""
        with open(path, "r") as f:
            data = json.load(f)
        messages = data.get("messages", [])
        entries = []
        for i, msg in enumerate(messages):
            role = classify_gemini(msg)
            timestamp = msg.get("timestamp", "")
            entries.append(IndexEntry(i, i, role, timestamp))
        return entries

    @staticmethod
    def _build_opencode_sqlite(db_path: str, session_id: str) -> tuple[list[IndexEntry], list[dict]]:
        """Build index from OpenCode SQLite database.

        Opens DB read-only. Returns (index entries, messages-with-parts cache).
        Messages-with-parts cache is used by read_raw() and the exchange builder.
        """
        conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True, timeout=5)
        conn.row_factory = sqlite3.Row

        try:
            messages = conn.execute(
                "SELECT id, session_id, time_created, data FROM message "
                "WHERE session_id = ? ORDER BY time_created ASC",
                (session_id,),
            ).fetchall()

            entries = []
            messages_with_parts = []
            for i, msg in enumerate(messages):
                data = json.loads(msg["data"])
                role = classify_opencode(data)
                ts = _epoch_ms_to_iso(msg["time_created"])
                entries.append(IndexEntry(i, i, role, ts))

                # Fetch parts for this message
                parts_rows = conn.execute(
                    "SELECT id, data FROM part WHERE message_id = ? ORDER BY id ASC",
                    (msg["id"],),
                ).fetchall()
                parts = []
                for p in parts_rows:
                    part_data = json.loads(p["data"])
                    part_data["id"] = p["id"]
                    parts.append(part_data)

                messages_with_parts.append({
                    "id": msg["id"],
                    "session_id": msg["session_id"],
                    "time_created": msg["time_created"],
                    "role": role,
                    "data": data,
                    "parts": parts,
                    "timestamp": ts,
                })
        finally:
            conn.close()

        return entries, messages_with_parts

    def user_entries(self) -> list[IndexEntry]:
        """Return entries with role 'user'."""
        return [e for e in self._entries if e.role == "user"]

    @property
    def _gemini_messages(self) -> list[dict]:
        """Lazy-cached Gemini messages array. Avoids re-reading JSON on every read_raw call."""
        if not hasattr(self, "_gemini_cache"):
            with open(self.path) as f:
                self._gemini_cache = json.load(f).get("messages", [])
        return self._gemini_cache

    def read_raw(self, entry: IndexEntry) -> dict:
        """Read and parse the raw JSON for an index entry."""
        if self.agent == "opencode":
            msgs = getattr(self, "_opencode_messages", [])
            if entry.line_no < len(msgs):
                return msgs[entry.line_no]
            return {}

        if self.agent == "gemini":
            messages = self._gemini_messages
            if entry.line_no < len(messages):
                return messages[entry.line_no]
            return {}

        # JSONL: seek to byte offset
        with open(self.path, "rb") as f:
            f.seek(entry.byte_offset)
            raw_line = f.readline()
            try:
                return json.loads(raw_line)
            except json.JSONDecodeError:
                return {}
