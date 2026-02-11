"""Public API for scripts and external tools."""

from __future__ import annotations

import json
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any, cast

# Single source of truth for error type
from .shared import HcomError, SenderIdentity

__all__ = ["HcomError", "Session", "session", "instances", "launch", "bundle"]


@dataclass
class Session:
    """Identity-bound session for hcom operations.

    Provides messaging, events, and transcript access tied to a specific
    instance identity. Data is fetched fresh on each call (no caching).

    Create via hcom.session():
        s = hcom.session()                    # auto-detect
        s = hcom.session(name="luna")        # explicit
        s = hcom.session(name="bot", external=True)  # external sender
    """

    _name: str
    _external: bool = False

    @property
    def name(self) -> str:
        """Instance name (e.g., 'luna' or 'worker-0')."""
        return self._name

    @property
    def info(self) -> dict:
        """Fresh instance info from database.

        Returns:
            Dict with keys:
                name (str): Full instance name (may include tag prefix).
                session_id (str): Claude session ID for transcript binding.
                connected (bool): True if instance exists in DB, False if external.
                directory (str): Working directory path.
                status (str): Current status ('active', 'listening', 'inactive').
                transcript_path (str): Path to transcript file.
                parent_name (str): Parent instance name (for subagents).
                tool (str): Tool type ('claude', 'gemini', 'codex').

        Raises:
            HcomError: If instance no longer exists.
        """
        if self._external:
            return {"name": self._name, "connected": False, "status": "external", "tool": "claude"}
        from .core.instances import load_instance_position, get_full_name

        data = load_instance_position(self._name)
        if not data:
            raise HcomError(f"Instance no longer exists: {self._name}")
        return {
            "name": get_full_name(data) or self._name,
            "session_id": data.get("session_id", ""),
            "connected": True,
            "directory": data.get("directory", ""),
            "status": data.get("status", "unknown"),
            "transcript_path": data.get("transcript_path", ""),
            "parent_name": data.get("parent_name", ""),
            "tool": data.get("tool", "claude"),
        }

    def _get_identity(self) -> SenderIdentity:
        """Get fresh identity for operations."""
        if self._external:
            return SenderIdentity(kind="external", name=self._name)
        from .core.instances import load_instance_position

        data = load_instance_position(self._name)
        if not data:
            raise HcomError(f"Instance no longer exists: {self._name}")
        return SenderIdentity(
            kind="instance",
            name=self._name,
            instance_data=cast(dict[str, Any], data),
            session_id=data.get("session_id"),
        )

    def send(
        self,
        message: str,
        *,
        to: str | None = None,
        intent: str | None = None,
        reply_to: str | None = None,
        thread: str | None = None,
        bundle: dict | None = None,
    ) -> list[str]:
        """Send message to instances.

        Args:
            message: Message text. Use @name or @prefix- for targeting.
            to: Target name (auto-prepends @name if not in message).
            intent: One of 'request', 'inform', 'ack'.
            reply_to: Event ID to reply to (required for intent='ack').
            thread: Thread name for grouping related messages.
            bundle: Bundle dict to create and attach. If provided, creates bundle event
                and appends bundle summary to message.

        Returns:
            List of instance names that received the message.

        Examples:
            s.send("@nova hello")
            s.send("@worker- start task", thread="batch-1", intent="request")
            s.send("received", to="luna", intent="ack", reply_to="42")

            # With bundle
            s.send("@reviewer check this", bundle={
                "title": "Code review",
                "description": "Auth module complete",
                "refs": {
                    "events": ["123-125"],
                    "files": ["auth.py"],
                    "transcript": ["10-15"]
                }
            })
        """
        from .core.ops import op_send
        from .core.helpers import validate_intent
        from .core.messages import resolve_reply_to, get_thread_from_event, MessageEnvelope

        if to and not message.lstrip().startswith(f"@{to}"):
            message = f"@{to} {message}"

        envelope: MessageEnvelope = {}
        if intent:
            try:
                validate_intent(intent)
            except ValueError as e:
                raise HcomError(str(e))
            envelope["intent"] = intent  # type: ignore[typeddict-item]

        if reply_to:
            local_id, error = resolve_reply_to(reply_to)
            if error:
                raise HcomError(f"Invalid reply_to: {error}")
            envelope["reply_to"] = reply_to
            if not thread and local_id:
                parent_thread = get_thread_from_event(local_id)
                if parent_thread:
                    thread = parent_thread

        if thread:
            envelope["thread"] = thread

        if intent == "ack" and "reply_to" not in envelope:
            raise HcomError("Intent 'ack' requires reply_to")

        # Handle bundle creation and attachment
        if bundle:
            from .core.bundles import create_bundle_event, validate_bundle

            try:
                validate_bundle(bundle)
            except ValueError as e:
                raise HcomError(str(e))

            # Determine instance name for bundle
            identity = self._get_identity()
            if identity.kind == "external":
                bundle_instance = f"ext_{identity.name}"
            elif identity.kind == "system":
                bundle_instance = f"sys_{identity.name}"
            else:
                bundle_instance = identity.name

            # Create bundle event
            bundle_id = create_bundle_event(bundle, instance=bundle_instance, created_by=identity.name)
            try:
                from .relay import notify_relay, push

                if not notify_relay():
                    push()
            except Exception:
                pass
            envelope["bundle_id"] = bundle_id

            # Append bundle summary to message
            refs = bundle.get("refs", {})
            events = refs.get("events", [])
            files = refs.get("files", [])
            transcript_refs = refs.get("transcript", [])
            extends = bundle.get("extends")

            def _join(vals):
                return ", ".join(str(v) for v in vals) if vals else ""

            bundle_lines = [
                f"[Bundle {bundle_id}]",
                f"Title: {bundle.get('title', '')}",
                f"Description: {bundle.get('description', '')}",
                "Refs:",
                f"  events: {_join(events)}",
                f"  files: {_join(files)}",
                f"  transcript: {_join(transcript_refs)}",
            ]
            if extends:
                bundle_lines.append(f"Extends: {extends}")

            sep = "\n\n" if message.strip() else ""
            message = message.rstrip() + sep + "\n".join(bundle_lines)

        return op_send(self._get_identity(), message, envelope=envelope if envelope else None)

    def messages(self, *, unread: bool = True, last: int = 20) -> list[dict]:
        """Get messages for this instance.

        Args:
            unread: If True, only messages delivered to this instance (mentions or
                broadcasts). If False, returns all messages in the system.
            last: Maximum number of messages to return (most recent first).

        Returns:
            List of dicts with keys:
                ts (str): ISO timestamp when message was sent.
                from (str): Sender's display name.
                text (str): Message text content.
                mentions (list[str]): Instance names mentioned in message.
                delivered_to (list[str]): Instance names message was delivered to.
                intent (str, optional): Message intent ('request', 'inform', 'ack').
                thread (str, optional): Thread name for grouping messages.
                reply_to (int, optional): Event ID this message replies to.
        """
        from .core.instances import load_instance_position, get_full_name

        if unread:
            # Safe JSON array membership check with parameter binding
            raw = self.events(
                sql="type='message' AND EXISTS (SELECT 1 FROM json_each(msg_delivered_to) WHERE value = ?)",
                params=[self._name],
                last=last,
            )
        else:
            raw = self.events(sql="type='message'", last=last)
        result = []
        for e in raw:
            data = e.get("data", {})
            sender_base = data.get("from", "")
            sender_data = load_instance_position(sender_base) if sender_base else None
            sender_display = get_full_name(sender_data) or sender_base
            result.append(
                {
                    "ts": e["ts"],
                    "from": sender_display,
                    "text": data.get("text", ""),
                    "mentions": data.get("mentions", []),
                    "delivered_to": data.get("delivered_to", []),
                    "intent": data.get("intent"),
                    "thread": data.get("thread"),
                    "reply_to": data.get("reply_to"),
                }
            )
        return result

    def events(
        self,
        *,
        sql: str | None = None,
        params: list | None = None,
        last: int = 20,
        # Structured filters (same semantics as CLI flags)
        agent: str | list[str] | None = None,
        type: str | None = None,
        file: str | list[str] | None = None,
        after: str | None = None,
        before: str | None = None,
        status: str | None = None,
        context: str | None = None,
        action: str | None = None,
        # Message filters
        sender: str | None = None,
        intent: str | None = None,
        thread: str | None = None,
    ) -> list[dict]:
        """Query the event stream.

        Filter kwargs mirror CLI flags (--agent, --file, --after, etc.).
        Same composition: different filters AND together, multiple values OR.
        Raw sql is ANDed with structured filters when both provided.

        Args:
            sql: Raw SQL WHERE clause (ANDed with structured filters).
            params: Parameters for SQL placeholders (?).
            last: Maximum events to return.
            agent: Instance name(s).
            type: Event type (message, status, life).
            file: File path pattern(s) — glob (*.py) or contains match.
            after: ISO-8601 timestamp lower bound.
            before: ISO-8601 timestamp upper bound.
            status: Status value (listening, active, blocked).
            context: Status context pattern (tool:Bash, deliver:*).
            action: Life action (created, ready, stopped).
            sender: Message sender name (maps to --from).
            intent: Message intent (request, inform, ack).
            thread: Message thread name.

        Returns:
            List of dicts with keys: ts, type, instance, data

        SQL fields (for raw sql= queries against events_v):
            Common: id, timestamp, type, instance
            Message: msg_from, msg_text, msg_thread, msg_intent,
                     msg_reply_to, msg_mentions, msg_delivered_to, msg_bundle_id
            Status: status_val, status_context, status_detail
            Lifecycle: life_action, life_by, life_batch_id

        Examples:
            s.events(file="src/core/*", after="2026-01-01T00:00:00")
            s.events(agent="nova", type="message")
            s.events(sender="luna", intent="request", last=50)
            s.events(sql="msg_from=?", params=["nova"])  # raw SQL still works
        """
        from .core.db import get_db
        from .core.filters import build_sql_from_flags

        # Build structured filter dict
        filters: dict[str, list] = {}
        for key, val in [
            ("instance", agent), ("type", type), ("file", file),
            ("after", after), ("before", before), ("status", status),
            ("context", context), ("action", action), ("from", sender),
            ("intent", intent), ("thread", thread),
        ]:
            if val is not None:
                filters[key] = val if isinstance(val, list) else [val]

        # Normalize tag-prefixed instance names (e.g., "review-luna" → "luna")
        if "instance" in filters:
            from .core.instances import resolve_display_name

            filters["instance"] = [
                resolve_display_name(n) or n for n in filters["instance"]
            ]

        try:
            structured_sql = build_sql_from_flags(filters) if filters else ""
        except ValueError as e:
            raise HcomError(str(e))

        query = "SELECT * FROM events_v WHERE 1=1"
        if structured_sql:
            query += f" AND {structured_sql}"
        if sql:
            query += f" AND ({sql})"
        query += f" ORDER BY id DESC LIMIT {last}"

        try:
            rows = get_db().execute(query, params or []).fetchall()
        except Exception as e:
            raise HcomError(f"SQL error: {e}")

        result = []
        for r in reversed(rows):
            try:
                result.append(
                    {
                        "ts": r["timestamp"],
                        "type": r["type"],
                        "instance": r["instance"],
                        "data": json.loads(r["data"]),
                    }
                )
            except (json.JSONDecodeError, TypeError):
                # Skip corrupt rows
                pass
        return result

    def wait(self, sql: str, *, params: list | None = None, timeout: int = 60) -> dict | None:
        """Block until an event matches the SQL condition.

        Args:
            sql: SQL WHERE clause to match.
            params: Parameters for SQL placeholders (?).
            timeout: Seconds to wait before returning None.

        Returns:
            Matching event dict, or None if timeout.

        Examples:
            event = s.wait("msg_from='nova'", timeout=60)
            event = s.wait("msg_thread=?", params=["task-1"], timeout=120)
        """
        from .core.db import get_db, get_last_event_id

        conn = get_db()
        params = params or []

        def _parse_row(row) -> dict | None:
            """Parse row, return None if corrupt."""
            try:
                return {
                    "ts": row["timestamp"],
                    "type": row["type"],
                    "instance": row["instance"],
                    "data": json.loads(row["data"]),
                }
            except (json.JSONDecodeError, TypeError):
                return None

        # 10s lookback for race conditions
        lookback_ts = datetime.fromtimestamp(time.time() - 10, tz=timezone.utc).isoformat()
        try:
            row = conn.execute(
                f"SELECT * FROM events_v WHERE timestamp > ? AND ({sql}) ORDER BY id DESC LIMIT 1",
                [lookback_ts] + params,
            ).fetchone()
        except Exception as e:
            raise HcomError(f"SQL error: {e}")

        if row:
            result = _parse_row(row)
            if result:
                return result

        start = time.time()
        last_id = get_last_event_id()
        while time.time() - start < timeout:
            try:
                row = conn.execute(
                    f"SELECT * FROM events_v WHERE id > ? AND ({sql}) ORDER BY id LIMIT 1",
                    [last_id] + params,
                ).fetchone()
            except Exception as e:
                raise HcomError(f"SQL error: {e}")

            if row:
                result = _parse_row(row)
                if result:
                    return result
            any_new = conn.execute("SELECT MAX(id) FROM events WHERE id > ?", [last_id]).fetchone()
            if any_new and any_new[0]:
                last_id = any_new[0]
            time.sleep(0.5)
        return None

    def subscribe(self, sql: str, *, params: list | None = None, once: bool = False) -> str:
        """Create a push subscription for events.

        When matching events occur, a notification is sent via hcom.

        Args:
            sql: SQL WHERE clause to match events.
            params: Parameters for SQL placeholders (?).
            once: If True, subscription auto-removes after first match.

        Returns:
            Subscription ID (e.g., 'sub-a1b2').

        Raises:
            HcomError: If called from external session (can't receive notifications).

        Examples:
            sub_id = s.subscribe("msg_thread='task-1'")
            sub_id = s.subscribe("msg_from=?", params=["nova"], once=True)
        """
        # External sessions can't receive notifications (no instances row)
        if self._external:
            raise HcomError("External sessions cannot subscribe to events (no notification delivery)")

        from .core.db import get_db, get_last_event_id, kv_set
        from hashlib import sha256

        conn = get_db()
        try:
            conn.execute(f"SELECT 1 FROM events_v WHERE ({sql}) LIMIT 0", params or [])
        except Exception as e:
            raise HcomError(f"Invalid SQL: {e}")

        now = time.time()
        sub_hash = sha256(f"{self._name}:{sql}:{now}".encode()).hexdigest()[:4]
        sub_id = f"sub-{sub_hash}"

        kv_set(
            f"events_sub:{sub_id}",
            json.dumps(
                {
                    "id": sub_id,
                    "sql": sql,
                    "params": params or [],
                    "caller": self._name,
                    "once": once,
                    "last_id": get_last_event_id(),
                    "created": now,
                }
            ),
        )
        return sub_id

    def subscriptions(self) -> list[dict]:
        """List all active event subscriptions.

        Returns:
            List of dicts with keys: id, sql, caller, once
        """
        from .core.db import get_db

        rows = get_db().execute("SELECT value FROM kv WHERE key LIKE 'events_sub:%'").fetchall()
        result = []
        for row in rows:
            try:
                data = json.loads(row["value"])
                result.append(
                    {
                        "id": data.get("id"),
                        "sql": data.get("sql"),
                        "caller": data.get("caller"),
                        "once": data.get("once", False),
                    }
                )
            except (json.JSONDecodeError, TypeError):
                pass
        return result

    def unsubscribe(self, sub_id: str) -> bool:
        """Remove an event subscription.

        Args:
            sub_id: Subscription ID (with or without 'sub-' prefix).

        Returns:
            True if removed, False if not found.
        """
        from .core.db import get_db, kv_set

        if not sub_id.startswith("sub-"):
            sub_id = f"sub-{sub_id}"
        key = f"events_sub:{sub_id}"
        if not get_db().execute("SELECT 1 FROM kv WHERE key = ?", (key,)).fetchone():
            return False
        kv_set(key, None)
        return True

    def transcript(
        self,
        agent: str,
        *,
        last: int = 10,
        full: bool = False,
        range: str | None = None,
        detailed: bool = False,
    ) -> list[dict]:
        """Get conversation transcript for an instance or timeline across all instances.

        Args:
            agent: Instance name, or "timeline" for timeline mode (all instances).
            last: Number of recent exchanges to return.
            full: If True, include truncated tool output (use detailed for full output).
            range: Exchange range like "5-10" (1-indexed, inclusive). Only valid for specific agent.
            detailed: If True, include full tool calls, results, file edits, errors.

        Returns:
            If agent is name: List of exchange dicts with keys: user, assistant, position, timestamp
            If agent is "timeline": List of entry dicts with keys: instance, position,
                user, action, timestamp, files, command

        Examples:
            s.transcript("nova")                     # nova's transcript
            s.transcript("nova", last=5)             # nova's last 5
            s.transcript("nova", range="1-10")       # nova's exchanges 1-10
            s.transcript("timeline", last=20)        # timeline across all agents
            s.transcript("timeline", detailed=True)  # detailed timeline
        """
        from .core.transcript import get_thread, get_timeline
        from .core.instances import load_instance_position
        from .core.db import get_db
        import re

        # Timeline mode: agent is "timeline"
        if agent == "timeline":
            if range:
                raise HcomError("range parameter not supported in timeline mode")

            # Get all instances with transcripts
            conn = get_db()
            active = conn.execute("""
                SELECT name, transcript_path, tool FROM instances
                WHERE transcript_path IS NOT NULL AND transcript_path != ''
            """).fetchall()

            stopped = conn.execute("""
                SELECT instance as name,
                       json_extract(data, '$.snapshot.transcript_path') as transcript_path,
                       json_extract(data, '$.snapshot.tool') as tool
                FROM events
                WHERE type = 'life' AND json_extract(data, '$.action') = 'stopped'
                  AND json_extract(data, '$.snapshot.transcript_path') IS NOT NULL
            """).fetchall()

            # Combine, deduping by name
            seen = set()
            instances = []
            for row in active:
                seen.add(row["name"])
                instances.append(
                    {
                        "name": row["name"],
                        "transcript_path": row["transcript_path"],
                        "tool": row["tool"] or "claude",
                    }
                )
            for row in stopped:
                if row["name"] not in seen:
                    seen.add(row["name"])
                    instances.append(
                        {
                            "name": row["name"],
                            "transcript_path": row["transcript_path"],
                            "tool": row["tool"] or "claude",
                        }
                    )

            result = get_timeline(instances, last=last, detailed=detailed or full)
            return result.get("entries", []) if not result.get("error") else []

        # Single agent mode - agent is required (not None)
        range_tuple = None
        if range:
            match = re.match(r"^(\d+)-(\d+)$", range)
            if not match:
                raise HcomError(f"Invalid range: {range}")
            start, end = int(match.group(1)), int(match.group(2))
            if start < 1 or end < 1 or start > end:
                raise HcomError(f"Invalid range: {range}")
            range_tuple = (start, end)

        data = load_instance_position(agent)
        if not data:
            raise HcomError(f"Instance not found: {agent}")

        transcript_path = data.get("transcript_path")
        if not transcript_path:
            return []

        tool = data.get("tool", "claude")
        result = get_thread(
            transcript_path,
            last=last,
            tool=tool,
            detailed=detailed or full,
            range_tuple=range_tuple,
        )
        return result.get("exchanges", []) if not result.get("error") else []

    def stop(self) -> None:
        """Stop this instance's hcom participation.

        The instance will no longer receive messages or appear in listings.
        """
        from .core.ops import op_stop

        op_stop(self._name, initiated_by=self._name)


# === Standalone Functions ===


def _ensure_init():
    from .core.db import init_db
    from .core.paths import ensure_hcom_directories

    ensure_hcom_directories()
    init_db()


def session(name: str | None = None, *, external: bool = False) -> Session:
    """Get an identity-bound session for hcom operations.

    Args:
        name: Instance name. Auto-detects from environment if None.
        external: If True, creates external sender (no instance required).

    Returns:
        Session object with messaging and event methods.

    Raises:
        HcomError: If name required but not provided or not found.

    Examples:
        s = hcom.session()                    # auto-detect
        s = hcom.session(name="luna")        # explicit instance
        s = hcom.session(name="ci", external=True)  # external sender
    """
    _ensure_init()

    if external:
        if not name:
            raise HcomError("External session requires name")
        return Session(_name=name, _external=True)

    from .core.identity import resolve_identity, resolve_from_name

    if name:
        identity = resolve_from_name(name)
    else:
        identity = resolve_identity()

    return Session(_name=identity.name, _external=False)


def instances(name: str | None = None) -> list[dict] | dict:
    """List active instances or get one by name.

    Args:
        name: Specific instance name, or None for all.

    Returns:
        If name: dict with keys name, session_id, status, directory, parent_name, tool
        If None: list of such dicts

    Raises:
        HcomError: If name specified but not found.

    Examples:
        all_instances = hcom.instances()
        nova = hcom.instances(name="nova")
    """
    from .core.db import iter_instances
    from .core.instances import load_instance_position, get_full_name, resolve_display_name

    _ensure_init()

    if name:
        # Resolve tag-prefixed names (e.g., "fatcow-luna" → "luna")
        resolved = resolve_display_name(name)
        if resolved:
            name = resolved
        data = load_instance_position(name)
        if not data:
            raise HcomError(f"Instance not found: {name}")
        return {
            "name": get_full_name(data) or name,
            "session_id": data.get("session_id", ""),
            "status": data.get("status", "unknown"),
            "directory": data.get("directory", ""),
            "parent_name": data.get("parent_name", ""),
            "tool": data.get("tool", "claude"),
        }

    return [
        {
            "name": get_full_name(d) or d["name"],
            "session_id": d.get("session_id", ""),
            "status": d.get("status", "unknown"),
            "directory": d.get("directory", ""),
            "parent_name": d.get("parent_name", ""),
            "tool": d.get("tool", "claude"),
        }
        for d in iter_instances()
    ]


def launch(
    count: int | list[dict] = 1,
    *,
    tool: str = "claude",
    tag: str | None = None,
    prompt: str | None = None,
    system_prompt: str | None = None,
    background: bool = False,
    claude_args: str | None = None,
    resume: str | None = None,
    fork: bool = False,
    tool_args: str | None = None,
    cwd: str | None = None,
    wait: bool = True,
    timeout: int = 30,
    batch_id: str | None = None,
    name: str | None = None,
) -> dict:
    """Launch AI tool instances.

    Two calling modes:

    Single launch:
        hcom.launch(3, tag="worker", prompt="do task")
        hcom.launch(1, tool="gemini", prompt="review code")

    Group launch (heterogeneous agents, shared batch):
        hcom.launch([
            {"tag": "confessor", "prompt": "...", "background": True},
            {"tool": "gemini", "tag": "calibrator", "prompt": "..."},
            {"tool": "gemini", "tag": "judge", "prompt": "..."},
        ])

    Args:
        count: Number of instances (int) or list of spec dicts.
        tool: One of 'claude', 'gemini', 'codex' (single mode).
        tag: Group tag (single mode).
        prompt: Initial prompt (single mode).
        system_prompt: System prompt override (single mode).
        background: If True, run headless (single mode).
        claude_args: Additional Claude CLI args (single mode).
        resume: Instance name or session ID to resume from (single mode).
            If an instance name (e.g., "luna"), loads snapshot automatically:
            sets tool, tag, cwd, background, system_prompt, and restores
            message delivery cursor. If a raw session ID, passes through as-is.
        fork: If True with resume, fork instead of continue (single mode).
            Fork creates a new instance from the same session history.
        tool_args: Additional tool-specific args (single mode).
        cwd: Working directory (single mode).
        wait: If True (default), block until all instances are ready or timeout.
        timeout: Max seconds to wait when wait=True.
        batch_id: Share a batch ID across multiple launch() calls for wait-for-all.
        name: Explicit instance name (single mode). Used for resume to reuse
            the stopped instance's name. Requires count=1 and name must not
            be in use by an active instance.

    Spec dict keys (group mode):
        count (int): Number of instances, default 1.
        tool (str): 'claude', 'gemini', or 'codex', default 'claude'.
        tag, prompt, system_prompt, background, claude_args,
        resume, fork, tool_args, cwd: Same as single mode args.

    Returns:
        Single mode: Dict with tool, batch_id, launched, failed, background,
            log_files, handles, errors, launch_status (if wait=True).
        Group mode: Dict with batch_id, results (list of per-spec dicts),
            total_launched, total_failed, launch_status (if wait=True).

    Raises:
        HcomError: On invalid tool, hook setup failure, or launch failure.
    """
    if isinstance(count, list):
        return _launch_group(count, wait=wait, timeout=timeout)

    return _launch_single(
        count,
        tool=tool,
        tag=tag,
        prompt=prompt,
        system_prompt=system_prompt,
        background=background,
        claude_args=claude_args,
        resume=resume,
        fork=fork,
        tool_args=tool_args,
        cwd=cwd,
        wait=wait,
        timeout=timeout,
        name=name,
        batch_id=batch_id,
    )


def _launch_group(
    specs: list[dict],
    *,
    wait: bool = True,
    timeout: int = 30,
) -> dict:
    """Launch heterogeneous agents from a list of spec dicts with a shared batch."""
    import uuid

    if not specs:
        raise HcomError("Empty specs list")

    _VALID_SPEC_KEYS = {
        "count", "tool", "tag", "prompt", "system_prompt", "background",
        "claude_args", "resume", "fork", "tool_args", "cwd",
    }

    shared_batch = str(uuid.uuid4()).split("-")[0]
    results = []
    total_launched = 0
    total_failed = 0

    for i, spec in enumerate(specs):
        if not isinstance(spec, dict):
            raise HcomError(f"Spec at index {i} must be a dict, got {type(spec).__name__}")
        bad_keys = set(spec) - _VALID_SPEC_KEYS
        if bad_keys:
            raise HcomError(f"Spec at index {i} has invalid keys: {bad_keys}")
        spec = dict(spec)  # copy to avoid mutation
        count = spec.pop("count", 1)
        # Never wait per-spec — we wait once at the end
        result = _launch_single(
            count,
            wait=False,
            batch_id=shared_batch,
            **spec,
        )
        results.append(result)
        total_launched += result.get("launched", 0)
        total_failed += result.get("failed", 0)

    out: dict[str, Any] = {
        "batch_id": shared_batch,
        "results": results,
        "total_launched": total_launched,
        "total_failed": total_failed,
    }

    if wait:
        _ensure_init()
        from .core.launch_status import wait_for_launch as _wait

        out["launch_status"] = _wait(batch_id=shared_batch, timeout=timeout)

    return out


def _resolve_resume_target(resume_val: str, *, fork: bool) -> dict | None:
    """If resume_val is an instance name (with or without tag prefix), resolve to snapshot data.

    Returns None if not an instance name (treated as raw session_id by caller).
    """
    from .core.instances import load_instance_position, resolve_display_name
    from .core.ops import load_stopped_snapshot, resume_system_prompt

    # Normalize tag-prefixed names (e.g., "fatcow-luna" → "luna")
    resolved_name = resolve_display_name(resume_val)
    name = resolved_name if resolved_name else resume_val

    # Try as active instance (fork allows active, resume does not)
    active = load_instance_position(name)
    if active:
        if not fork:
            raise HcomError(f"'{resume_val}' is still active — stop it first, or pass a raw session_id")
        session_id = active.get("session_id")
        if not session_id:
            raise HcomError(f"'{resume_val}' has no session_id")
        tool = active.get("tool", "claude")
        if tool not in ("claude", "codex"):
            raise HcomError(f"Fork not supported for {tool}")
        return {
            "name": name,
            "session_id": session_id,
            "tool": tool,
            "tag": active.get("tag"),
            "directory": active.get("directory"),
            "background": bool(active.get("background")),
            "last_event_id": active.get("last_event_id"),
            "system_prompt": resume_system_prompt(name, fork=True),
        }

    # Try as stopped instance
    snapshot = load_stopped_snapshot(name)
    if not snapshot:
        return None  # Not an instance name — treat as raw session_id

    session_id = snapshot.get("session_id")
    if not session_id:
        raise HcomError(f"'{resume_val}' has no session_id")

    tool = snapshot.get("tool", "claude")
    if fork and tool not in ("claude", "codex"):
        raise HcomError(f"Fork not supported for {tool}")

    return {
        "name": name,
        "session_id": session_id,
        "tool": tool,
        "tag": snapshot.get("tag"),
        "directory": snapshot.get("directory"),
        "background": bool(snapshot.get("background")),
        "last_event_id": snapshot.get("last_event_id"),
        "system_prompt": resume_system_prompt(name, fork=fork),
    }


def _restore_resume_cursor(resolved: dict | None, fork: bool, result: dict) -> None:
    """Restore last_event_id cursor after instance-name resume (not fork).

    This ensures messages sent while the instance was stopped get delivered.
    """
    if not resolved or fork or result.get("launched", 0) == 0:
        return
    cursor = resolved.get("last_event_id")
    if cursor is not None:
        from .core.instances import update_instance_position
        update_instance_position(resolved["name"], {"last_event_id": cursor})


def _launch_single(
    count: int = 1,
    *,
    tool: str = "claude",
    tag: str | None = None,
    prompt: str | None = None,
    system_prompt: str | None = None,
    background: bool = False,
    claude_args: str | None = None,
    resume: str | None = None,
    fork: bool = False,
    tool_args: str | None = None,
    cwd: str | None = None,
    wait: bool = True,
    timeout: int = 30,
    batch_id: str | None = None,
    name: str | None = None,
) -> dict:
    """Launch instances of a single tool. Internal — called by launch()."""
    from .core.config import get_config
    from .launcher import launch as unified_launch
    import shlex

    _ensure_init()
    config = get_config()

    # Resolve instance-name-based resume: if resume is an instance name (active or
    # stopped), load its snapshot and fill in tool/tag/cwd/background/system_prompt/name.
    # If it's a raw session_id, _resolve_resume_target returns None and we proceed as before.
    _resolved = None
    if resume:
        _resolved = _resolve_resume_target(resume, fork=fork)
        if _resolved:
            resume = _resolved["session_id"]
            tool = _resolved["tool"]
            if tag is None:
                tag = _resolved.get("tag")
            if cwd is None:
                cwd = _resolved.get("directory")
            if not background and _resolved.get("background"):
                background = True
            if system_prompt is None:
                system_prompt = _resolved["system_prompt"]
            if name is None and not fork:
                name = _resolved["name"]

    if tool in ("claude", "claude-pty"):
        from .tools.claude.args import (
            resolve_claude_args,
            merge_claude_args,
            add_background_defaults,
        )

        args_list = []
        if resume:
            args_list.extend(["--resume", resume])
        if fork:
            args_list.append("--fork-session")
        if claude_args:
            args_list.extend(shlex.split(claude_args))
        # Add system_prompt AFTER claude_args so it has highest precedence (last wins)
        # Only add if truthy (empty string means "don't set system prompt")
        if system_prompt:
            args_list.extend(["--system-prompt", system_prompt])

        env_spec = resolve_claude_args(None, config.claude_args)
        cli_spec = resolve_claude_args(args_list if args_list else None, None)

        spec = (
            merge_claude_args(env_spec, cli_spec) if (cli_spec.clean_tokens or cli_spec.positional_tokens) else env_spec
        )

        if prompt is not None or background:
            spec = spec.update(
                prompt=prompt,
                background=background if background else None,
            )

        from .shared import (
            skip_tool_args_validation,
            HCOM_SKIP_TOOL_ARGS_VALIDATION_ENV,
        )

        if spec.has_errors() and not skip_tool_args_validation():
            raise HcomError(
                "\n".join(
                    [
                        *spec.errors,
                        f"Tip: set {HCOM_SKIP_TOOL_ARGS_VALIDATION_ENV}=1 to bypass hcom "
                        f"validation and let claude handle args.",
                    ]
                )
            )

        spec = add_background_defaults(spec)

        # PTY mode: use PTY wrapper for interactive Claude (not headless, not Windows)
        # Match CLI behavior (cmd_launch): PTY for interactive, not headless
        from .shared import IS_WINDOWS

        use_pty = (tool == "claude-pty") or (not spec.is_background and not IS_WINDOWS)

        result = unified_launch(
            tool,
            count,
            spec.rebuild_tokens(),
            tag=tag or config.tag,
            background=spec.is_background,
            pty=use_pty,
            launcher="api",
            cwd=cwd,
            batch_id=batch_id,
            name=name,
        )
        if wait and result.get("batch_id"):
            from .core.launch_status import wait_for_launch

            result["launch_status"] = wait_for_launch(
                batch_id=result["batch_id"], timeout=timeout
            )
        _restore_resume_cursor(_resolved, fork, result)
        return result

    if tool not in ("gemini", "codex"):
        raise HcomError(f"Unknown tool: {tool}")

    # Hook setup moved to launcher.launch() - single source of truth

    args_list = []
    if resume:
        if tool == "codex":
            # Codex: use 'fork' subcommand if fork=True, else 'resume'
            subcommand = "fork" if fork else "resume"
            args_list = [subcommand, resume]
        else:
            args_list = ["--resume", resume]
    if tool_args:
        args_list.extend(shlex.split(tool_args))

    # Merge env args (HCOM_CODEX_ARGS / HCOM_GEMINI_ARGS) with CLI args
    if tool == "gemini":
        from .tools.gemini.args import resolve_gemini_args, merge_gemini_args

        gemini_env_spec = resolve_gemini_args(None, config.gemini_args)
        gemini_cli_spec = resolve_gemini_args(args_list if args_list else None, None)
        gemini_spec = (
            merge_gemini_args(gemini_env_spec, gemini_cli_spec)
            if (gemini_cli_spec.clean_tokens or gemini_cli_spec.positional_tokens)
            else gemini_env_spec
        )
        if prompt:
            gemini_spec = gemini_spec.update(prompt=prompt)
        args_list = gemini_spec.rebuild_tokens()
    elif tool == "codex":
        from .tools.codex.args import resolve_codex_args, merge_codex_args

        codex_env_spec = resolve_codex_args(None, config.codex_args)
        codex_cli_spec = resolve_codex_args(args_list if args_list else None, None)
        codex_spec = (
            merge_codex_args(codex_env_spec, codex_cli_spec)
            if (codex_cli_spec.clean_tokens or codex_cli_spec.positional_tokens or codex_cli_spec.subcommand)
            else codex_env_spec
        )
        if prompt:
            codex_spec = codex_spec.update(prompt=prompt)
        args_list = codex_spec.rebuild_tokens()

    result = unified_launch(
        tool,
        count,
        args_list,
        tag=tag,
        system_prompt=system_prompt,
        background=background,
        launcher="api",
        cwd=cwd,
        batch_id=batch_id,
        name=name,
    )
    if wait and result.get("batch_id"):
        from .core.launch_status import wait_for_launch

        result["launch_status"] = wait_for_launch(
            batch_id=result["batch_id"], timeout=timeout
        )
    _restore_resume_cursor(_resolved, fork, result)
    return result


def wait_for_launch(
    batch_id: str | None = None,
    timeout: int = 30,
) -> dict:
    """Wait for a launch batch to be ready.

    Prefer using launch(specs_list, wait=True) for group launches.
    This function remains for cases where you need manual batch_id coordination.

    Args:
        batch_id: Batch ID from launch(batch_id=X, wait=False) calls.
        timeout: Max seconds to wait.

    Returns:
        Dict with status ("ready"|"timeout"|"error"|"no_launches"), expected, ready, instances.
    """
    _ensure_init()
    from .core.launch_status import wait_for_launch as _wait
    return _wait(batch_id=batch_id, timeout=timeout)


def bundle(
    action: str = "list",
    *,
    # Create args
    title: str | None = None,
    description: str | None = None,
    events: list[str] | None = None,
    files: list[str] | None = None,
    transcript: list[str] | None = None,
    extends: str | None = None,
    data: dict | None = None,
    # Show/Chain args
    bundle_id: str | None = None,
    # List args
    last: int = 20,
) -> list[dict] | dict | str:
    """Manage bundles for context handoff and review workflows.

    Bundles package conversation transcript ranges, event IDs, and file paths
    into referenceable context units for handoffs between agents.

    Args:
        action: One of 'list', 'show', 'create', 'chain'.
        title: Title for new bundle.
        description: Description for new bundle.
        events: List of event IDs/ranges for new bundle (e.g., ["123-125", "130"]).
        files: List of file paths for new bundle.
        transcript: List of transcript ranges for new bundle (e.g., ["5-10", "15"]).
        extends: Parent bundle ID for chaining related work.
        data: Full bundle dict (alternative to separate fields).
        bundle_id: ID for show/chain actions.
        last: Limit for list action.

    Returns:
        list (for 'list', 'chain'): List of bundle dicts.
        dict (for 'show'): Bundle details.
        str (for 'create'): New bundle ID.

    Examples:
        # Create a bundle
        bundle_id = hcom.bundle("create",
            title="Code review: auth module",
            description="Implementation complete, ready for review",
            events=["123-125", "130"],
            files=["auth.py", "tests/test_auth.py"],
            transcript=["10-15"]
        )

        # List recent bundles
        bundles = hcom.bundle("list", last=10)

        # Get bundle details
        details = hcom.bundle("show", bundle_id="abc123")

        # Get bundle chain (all related bundles)
        chain = hcom.bundle("chain", bundle_id="abc123")
    """
    _ensure_init()
    from .core.db import get_db

    conn = get_db()

    # Helper to find bundle by ID or bundle_id prefix (shared by show and chain)
    def _get(bid):
        if bid.isdigit():
            return conn.execute(
                "SELECT id, timestamp, data FROM events WHERE type = 'bundle' AND id = ?",
                (int(bid),),
            ).fetchone()
        return conn.execute(
            """
            SELECT id, timestamp, data
            FROM events
            WHERE type = 'bundle'
              AND json_extract(data, '$.bundle_id') LIKE ?
            ORDER BY id DESC LIMIT 1
            """,
            (f"{bid}%",),
        ).fetchone()

    if action == "list":
        rows = conn.execute(
            """
            SELECT id, timestamp,
                   json_extract(data, '$.bundle_id') as bundle_id,
                   json_extract(data, '$.title') as title,
                   json_extract(data, '$.description') as description,
                   json_extract(data, '$.created_by') as created_by,
                   json_extract(data, '$.refs.events') as events
            FROM events
            WHERE type = 'bundle'
            ORDER BY id DESC
            LIMIT ?
            """,
            (last,),
        ).fetchall()

        bundles = []
        for r in rows:
            bundles.append(
                {
                    "id": r["id"],
                    "timestamp": r["timestamp"],
                    "bundle_id": r["bundle_id"],
                    "title": r["title"],
                    "description": r["description"],
                    "created_by": r["created_by"],
                    "events": json.loads(r["events"]) if r["events"] else [],
                }
            )
        return bundles

    if action == "show":
        if not bundle_id:
            raise HcomError("bundle_id required for 'show'")

        row = _get(bundle_id)
        if not row:
            raise HcomError(f"Bundle not found: {bundle_id}")

        res = json.loads(row["data"]) if row["data"] else {}
        res["event_id"] = row["id"]
        res["timestamp"] = row["timestamp"]
        return res

    if action == "create":
        from .core.bundles import create_bundle_event, validate_bundle
        from .core.identity import resolve_identity

        try:
            identity = resolve_identity()
        except Exception:
            raise HcomError("Cannot create bundle without identity (run hcom start)")

        if data:
            bundle_data = data
        else:
            if not title:
                raise HcomError("Title required")
            if not description:
                raise HcomError("Description required")

            bundle_data = {
                "title": title,
                "description": description,
                "refs": {
                    "events": events or [],
                    "files": files or [],
                    "transcript": transcript or [],
                },
            }
            if extends:
                bundle_data["extends"] = extends

        try:
            validate_bundle(bundle_data)
        except ValueError as e:
            raise HcomError(str(e))

        # Determine instance name
        if identity.kind == "external":
            inst = f"ext_{identity.name}"
        elif identity.kind == "system":
            inst = f"sys_{identity.name}"
        else:
            inst = identity.name

        bundle_id = create_bundle_event(bundle_data, instance=inst, created_by=identity.name)
        try:
            from .relay import notify_relay, push

            if not notify_relay():
                push()
        except Exception:
            pass
        return bundle_id

    if action == "chain":
        if not bundle_id:
            raise HcomError("bundle_id required for 'chain'")

        chain = []
        curr = _get(bundle_id)
        if not curr:
            raise HcomError(f"Bundle not found: {bundle_id}")

        # Track seen bundle_ids to detect cycles
        seen = set()

        while curr:
            b_data = json.loads(curr["data"]) if curr["data"] else {}
            curr_bundle_id = b_data.get("bundle_id")

            # Cycle detection: stop if we've seen this bundle before
            if curr_bundle_id in seen:
                break
            seen.add(curr_bundle_id)

            b_data["event_id"] = curr["id"]
            b_data["timestamp"] = curr["timestamp"]
            chain.append(b_data)

            parent = b_data.get("extends")
            if not parent:
                break
            curr = _get(parent)

        return chain

    raise HcomError(f"Unknown action: {action}")
