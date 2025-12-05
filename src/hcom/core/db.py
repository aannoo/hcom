"""SQLite event storage - unified database for messages, status, and lifecycle events"""
from __future__ import annotations
import sqlite3
import json
import os
import threading
from typing import Any, Optional
from datetime import datetime, timezone

from .paths import hcom_path
from ..shared import parse_iso_timestamp

# Database configuration
DB_FILE = "hcom.db"
_thread_local = threading.local()  # Per-thread connection storage
_write_lock = threading.Lock()  # Protect concurrent writes

# ==================== Connection Management ====================

def get_db() -> sqlite3.Connection:
    """Get thread-local database connection, creating if needed.

    Returns per-thread connection with WAL mode enabled for concurrent access.
    Each thread gets its own connection to avoid SQLite threading issues.

    Detects if DB file was deleted/replaced (e.g., by `hcom reset logs`) and
    reconnects automatically.
    """
    db_path = hcom_path(DB_FILE)

    # Check if existing connection is stale (DB file deleted or replaced)
    if hasattr(_thread_local, 'conn') and _thread_local.conn is not None:
        if hasattr(_thread_local, 'db_inode'):
            try:
                current_inode = os.stat(db_path).st_ino if db_path.exists() else None
                if current_inode != _thread_local.db_inode:
                    # DB file changed - close stale connection
                    try:
                        _thread_local.conn.close()
                    except Exception:
                        pass
                    _thread_local.conn = None
                    _thread_local.db_inode = None
            except Exception:
                pass

    if not hasattr(_thread_local, 'conn') or _thread_local.conn is None:
        _thread_local.conn = sqlite3.connect(str(db_path))
        _thread_local.conn.row_factory = sqlite3.Row

        # Track inode for stale connection detection
        try:
            _thread_local.db_inode = os.stat(db_path).st_ino
        except Exception:
            _thread_local.db_inode = None

        # Enable foreign key constraints (disabled by default in SQLite)
        _thread_local.conn.execute("PRAGMA foreign_keys = ON")

        # Enable WAL mode for concurrent reads/writes
        _thread_local.conn.execute("PRAGMA journal_mode=WAL")
        _thread_local.conn.execute("PRAGMA wal_autocheckpoint=1000")
        _thread_local.conn.execute("PRAGMA busy_timeout=5000")

        init_db(_thread_local.conn)

    return _thread_local.conn

def close_db() -> None:
    """Close thread-local database connection and clear cache.

    Idempotent - safe to call multiple times or when no connection exists.
    Only closes connection for current thread.
    """
    if hasattr(_thread_local, 'conn') and _thread_local.conn is not None:
        _thread_local.conn.close()
        _thread_local.conn = None

# ==================== Schema Management ====================

def init_db(conn: Optional[sqlite3.Connection] = None) -> None:
    """Create database schema if not exists. Idempotent.

    Schema:
        events(id, timestamp, type, instance, data)
        - id: autoincrement primary key for position tracking
        - timestamp: ISO 8601 datetime for time-based queries
        - type: event type ('message', 'status', 'life')
        - instance: instance alias (sender for messages, subject for status/life)
        - data: JSON blob with type-specific fields

        instances(name, session_id, parent_session_id, ...)
        - name: instance alias (primary key)
        - session_id: unique session identifier (NULL for vanilla)
        - parent_session_id: parent's session_id for subagents

    Indexes:
        - timestamp for time-range queries
        - type for filtering by event type
        - instance for per-instance queries
        - type+instance composite for common filtered queries
        - session_id, parent_session_id, enabled, created_at, status, mapid

    Note: Foreign key constraints are enabled per-connection in get_db(),
    not here (PRAGMA settings are connection-specific).
    """
    if conn is None:
        conn = get_db()

    # Create events table
    conn.execute("""
        CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            type TEXT NOT NULL,
            instance TEXT NOT NULL,
            data TEXT NOT NULL
        )
    """)

    # Create instances table with JSON columns for complex fields
    conn.execute("""
        CREATE TABLE IF NOT EXISTS instances (
            name TEXT PRIMARY KEY,
            session_id TEXT UNIQUE,
            mapid TEXT,
            parent_session_id TEXT,
            parent_name TEXT,
            tag TEXT,
            last_event_id INTEGER DEFAULT 0,
            enabled INTEGER DEFAULT 0,
            status TEXT DEFAULT 'unknown',
            status_time INTEGER DEFAULT 0,
            status_context TEXT DEFAULT '',
            last_stop INTEGER DEFAULT 0,
            directory TEXT,
            created_at REAL NOT NULL,
            transcript_path TEXT DEFAULT '',
            tcp_mode INTEGER DEFAULT 0,
            wait_timeout INTEGER DEFAULT 1800,
            notify_port INTEGER,
            background INTEGER DEFAULT 0,
            background_log_file TEXT DEFAULT '',
            alias_announced INTEGER DEFAULT 0,
            launch_context_announced INTEGER DEFAULT 0,
            external_stop_pending INTEGER DEFAULT 0,
            session_ended INTEGER DEFAULT 0,
            agent_id TEXT UNIQUE,
            FOREIGN KEY (parent_session_id) REFERENCES instances(session_id) ON DELETE SET NULL
        )
    """)

    # Create MAPID → session_id mapping table for Windows identity resolution
    conn.execute("""
        CREATE TABLE IF NOT EXISTS mapid_sessions (
            mapid TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            updated_at REAL NOT NULL
        )
    """)

    # Migrate existing databases: add mapid column if missing
    cursor = conn.execute("PRAGMA table_info(instances)")
    columns = {row['name'] for row in cursor.fetchall()}
    if 'mapid' not in columns:
        conn.execute("ALTER TABLE instances ADD COLUMN mapid TEXT DEFAULT ''")
        conn.commit()

    # Migrate existing databases: add agent_id column if missing
    cursor = conn.execute("PRAGMA table_info(instances)")
    columns = {row['name'] for row in cursor.fetchall()}
    if 'agent_id' not in columns:
        try:
            conn.execute("ALTER TABLE instances ADD COLUMN agent_id TEXT")
            conn.commit()
        except sqlite3.OperationalError as e:
            # Concurrent process may have added it - ignore if duplicate column error
            if 'duplicate column' not in str(e).lower():
                raise

    # Migrate existing databases: add running_tasks column if missing
    cursor = conn.execute("PRAGMA table_info(instances)")
    columns = {row['name'] for row in cursor.fetchall()}
    if 'running_tasks' not in columns:
        try:
            conn.execute("ALTER TABLE instances ADD COLUMN running_tasks TEXT DEFAULT ''")
            conn.commit()
        except sqlite3.OperationalError as e:
            if 'duplicate column' not in str(e).lower():
                raise

    # Create indexes for common query patterns
    conn.execute("CREATE INDEX IF NOT EXISTS idx_timestamp ON events(timestamp)")
    conn.execute("CREATE INDEX IF NOT EXISTS idx_type ON events(type)")
    conn.execute("CREATE INDEX IF NOT EXISTS idx_instance ON events(instance)")
    conn.execute("CREATE INDEX IF NOT EXISTS idx_type_instance ON events(type, instance)")

    # Create instance indexes
    conn.execute("CREATE INDEX IF NOT EXISTS idx_session_id ON instances(session_id)")
    conn.execute("CREATE INDEX IF NOT EXISTS idx_parent_session_id ON instances(parent_session_id)")
    conn.execute("CREATE INDEX IF NOT EXISTS idx_parent_name ON instances(parent_name)")
    conn.execute("CREATE INDEX IF NOT EXISTS idx_enabled ON instances(enabled)")
    conn.execute("CREATE INDEX IF NOT EXISTS idx_created_at ON instances(created_at DESC)")
    conn.execute("CREATE INDEX IF NOT EXISTS idx_status ON instances(status)")
    conn.execute("CREATE INDEX IF NOT EXISTS idx_mapid ON instances(mapid) WHERE mapid != ''")
    conn.execute("CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_id_unique ON instances(agent_id) WHERE agent_id IS NOT NULL")

    # Create mapid_sessions index
    conn.execute("CREATE INDEX IF NOT EXISTS idx_mapid_sessions_updated ON mapid_sessions(updated_at DESC)")

    conn.commit()

    # Run schema migration (idempotent)
    migrate_sync_schema(conn)

def migrate_sync_schema(conn: Optional[sqlite3.Connection] = None) -> None:
    """Run schema migration (idempotent). Adds origin_device_id and kv table."""
    if conn is None:
        conn = get_db()

    version = conn.execute("PRAGMA user_version").fetchone()[0]

    if version < 2:
        # Add origin_device_id to instances
        with _write_lock:
            columns = [row[1] for row in conn.execute("PRAGMA table_info(instances)")]
            if 'origin_device_id' not in columns:
                try:
                    conn.execute("ALTER TABLE instances ADD COLUMN origin_device_id TEXT DEFAULT ''")
                    conn.execute("CREATE INDEX IF NOT EXISTS idx_instances_origin ON instances(origin_device_id)")
                    conn.commit()
                except sqlite3.OperationalError as e:
                    # Concurrent process may have added it
                    if 'duplicate column' not in str(e).lower():
                        raise

        with _write_lock:
            conn.execute("PRAGMA user_version = 2")
            conn.commit()

    # kv table for relay state
    with _write_lock:
        conn.execute("CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT)")
        conn.commit()

# ==================== Event Operations ====================

def log_event(
    event_type: str,
    instance: str,
    data: dict[str, Any],
    timestamp: Optional[str] = None
) -> int:
    """Insert event and return its ID.

    Thread-safe: Uses lock to protect concurrent writes.

    Args:
        event_type: Event type ('message', 'status', 'life')
        instance: Instance alias (sender for messages, subject for status/life)
        data: Type-specific event data
        timestamp: Optional ISO 8601 timestamp (defaults to now)

    Returns:
        Event ID (autoincrement primary key)
    """
    conn = get_db()
    if timestamp is None:
        timestamp = datetime.now(timezone.utc).isoformat()

    with _write_lock:
        cursor = conn.execute(
            "INSERT INTO events (timestamp, type, instance, data) VALUES (?, ?, ?, ?)",
            (timestamp, event_type, instance, json.dumps(data))
        )
        conn.commit()
        event_id = cursor.lastrowid

    # Sync export moved to hook dispatcher (rate-limited, builds from SQLite)
    # No duplicate JSONL append needed - single source of truth
    return event_id

def get_events_since(
    last_event_id: int = 0,
    event_type: Optional[str] = None,
    instance: Optional[str] = None
) -> list[dict[str, Any]]:
    """Query events by ID position with optional filters.

    Args:
        last_event_id: Return events with ID > this value (0 = all events)
        event_type: Optional filter by event type
        instance: Optional filter by instance

    Returns:
        List of events ordered by ID, each with: id, timestamp, type, instance, data (parsed JSON)
    """
    conn = get_db()

    query = "SELECT id, timestamp, type, instance, data FROM events WHERE id > ?"
    params: list[Any] = [last_event_id]

    if event_type is not None:
        query += " AND type = ?"
        params.append(event_type)

    if instance is not None:
        query += " AND instance = ?"
        params.append(instance)

    query += " ORDER BY id"

    cursor = conn.execute(query, params)
    return [
        {
            "id": row["id"],
            "timestamp": row["timestamp"],
            "type": row["type"],
            "instance": row["instance"],
            "data": json.loads(row["data"])
        }
        for row in cursor.fetchall()
    ]

def get_last_event_id() -> int:
    """Get current maximum event ID.

    Returns:
        Maximum event ID, or 0 if no events exist
    """
    conn = get_db()
    cursor = conn.execute("SELECT MAX(id) FROM events")
    result = cursor.fetchone()[0]
    return result if result is not None else 0

# ==================== Instance Operations ====================

def get_instance(name: str) -> dict[str, Any] | None:
    """Get instance by name. Returns dict or None."""
    conn = get_db()
    row = conn.execute("SELECT * FROM instances WHERE name = ?", (name,)).fetchone()
    if not row:
        return None

    # Convert Row to dict
    return dict(row)

def save_instance(name: str, data: dict[str, Any]) -> bool:
    """Insert or update instance using UPSERT. Returns True on success."""
    conn = get_db()

    try:
        with _write_lock:
            data_copy = data.copy()

            # UPSERT - simpler and race-free
            columns = ', '.join(data_copy.keys())
            placeholders = ', '.join('?' * len(data_copy))
            update_clause = ', '.join(f"{k} = excluded.{k}" for k in data_copy.keys() if k != 'name')

            conn.execute(
                f"""
                INSERT INTO instances ({columns}) VALUES ({placeholders})
                ON CONFLICT(name) DO UPDATE SET {update_clause}
                """,
                tuple(data_copy.values())
            )

            conn.commit()
            return True
    except sqlite3.Error as e:
        import sys
        print(f"DB error saving instance {name}: {e}", file=sys.stderr)
        return False
    except Exception as e:
        import sys
        print(f"Unexpected error saving instance {name}: {e}", file=sys.stderr)
        return False

def update_instance(name: str, updates: dict[str, Any]) -> bool:
    """Update specific instance fields. Returns True on success."""
    if not updates:
        return True

    conn = get_db()

    try:
        with _write_lock:
            updates_copy = updates.copy()

            # Simple UPDATE
            set_clause = ', '.join(f"{k} = ?" for k in updates_copy.keys())
            conn.execute(
                f"UPDATE instances SET {set_clause} WHERE name = ?",
                (*updates_copy.values(), name)
            )

            conn.commit()
            return True
    except sqlite3.Error as e:
        import sys
        print(f"DB error updating instance {name}: {e}", file=sys.stderr)
        return False
    except Exception as e:
        import sys
        print(f"Unexpected error updating instance {name}: {e}", file=sys.stderr)
        return False

def delete_instance(name: str) -> bool:
    """Delete instance from database. Returns True on success."""
    conn = get_db()

    try:
        with _write_lock:
            conn.execute("DELETE FROM instances WHERE name = ?", (name,))
            conn.commit()
            return True
    except sqlite3.Error as e:
        import sys
        print(f"DB error deleting instance {name}: {e}", file=sys.stderr)
        return False
    except Exception as e:
        import sys
        print(f"Unexpected error deleting instance {name}: {e}", file=sys.stderr)
        return False

def find_instance_by_session(session_id: str) -> str | None:
    """Find instance name by session_id. Returns name or None.

    Note: Vanilla instances have session_id=NULL, not matched by this lookup.
    """
    if not session_id:
        return None  # Don't match empty/None session_id

    conn = get_db()
    row = conn.execute("SELECT name FROM instances WHERE session_id = ?", (session_id,)).fetchone()
    return row['name'] if row else None

def get_instance_by_mapid(mapid: str) -> dict[str, Any] | None:
    """Get instance by mapid (launch token or WT_SESSION). Returns latest if multiple match.

    Note: Multiple instances can share same mapid (terminal reused for new conversations).
    Returns latest by created_at.
    """
    if not mapid:
        return None

    conn = get_db()
    row = conn.execute(
        "SELECT * FROM instances WHERE mapid = ? ORDER BY created_at DESC LIMIT 1",
        (mapid,)
    ).fetchone()

    if not row:
        return None

    # Convert Row to dict
    return dict(row)

# ==================== High-Level Query Helpers ====================

def iter_instances(enabled_only: bool = False):
    """Iterate instances efficiently (generator, not giant dict)."""
    conn = get_db()
    query = "SELECT * FROM instances"
    if enabled_only:
        query += " WHERE enabled = 1"
    query += " ORDER BY created_at DESC"

    for row in conn.execute(query):
        yield dict(row)

# ==================== Launch Batch Queries ====================

LAUNCH_TIMEOUT_SECONDS = 30  # Timeout for batch to become ready

def get_launch_status(launcher: str | None = None) -> dict | None:
    """Get aggregated launch status across all pending/recent batches.

    Priority:
    1. All pending batches (ready < expected) by launcher
    2. All batches from last 60s by launcher
    3. Most recent batch by launcher
    4. None

    Returns:
        Dict with: expected, ready, instances, batches (list), launcher
        Or None if no launches found.
    """
    conn = get_db()
    from datetime import datetime, timezone

    launcher_filter = "AND e.instance = ?" if launcher else ""
    params = (launcher,) if launcher else ()

    # Get all launch events by this launcher
    launches = conn.execute(f"""
        SELECT e.timestamp, e.instance as launcher,
               json_extract(e.data, '$.batch_id') as batch_id,
               json_extract(e.data, '$.launched') as expected
        FROM events e
        WHERE e.type = 'life'
          AND json_extract(e.data, '$.action') = 'launched'
          {launcher_filter}
        ORDER BY e.id DESC
        LIMIT 20
    """, params).fetchall()

    if not launches:
        return None

    # Get ready counts per batch
    def get_ready(batch_id: str) -> tuple[int, list[str]]:
        rows = conn.execute("""
            SELECT instance FROM events
            WHERE type = 'life'
              AND json_extract(data, '$.action') = 'ready'
              AND json_extract(data, '$.batch_id') = ?
        """, (batch_id,)).fetchall()
        return len(rows), [r['instance'] for r in rows]

    # Build batch info with ready counts
    batches = []
    for row in launches:
        ready_count, ready_instances = get_ready(row['batch_id'])
        batches.append({
            'batch_id': row['batch_id'],
            'launcher': row['launcher'],
            'expected': row['expected'] or 0,
            'ready': ready_count,
            'instances': ready_instances,
            'timestamp': row['timestamp'],
            'pending': ready_count < (row['expected'] or 0),
        })

    # Use provided launcher or get from first batch
    effective_launcher = launcher or batches[0]['launcher']

    # Cutoff for "recent" - successful launch should be ready by then
    cutoff = datetime.now(timezone.utc).timestamp() - LAUNCH_TIMEOUT_SECONDS

    # Priority 1: pending batches (incomplete AND recent - timeout stuck launches)
    pending = []
    for b in batches:
        if b['pending']:
            dt = parse_iso_timestamp(b['timestamp'])
            if dt and dt.timestamp() > cutoff:
                pending.append(b)
    if pending:
        return _aggregate_batches(pending, effective_launcher)

    # Priority 2: batches from last 60s
    recent = []
    for b in batches:
        dt = parse_iso_timestamp(b['timestamp'])
        if dt and dt.timestamp() > cutoff:
            recent.append(b)
    if recent:
        return _aggregate_batches(recent, effective_launcher)

    # Priority 3: most recent
    return _aggregate_batches([batches[0]], effective_launcher)


def _aggregate_batches(batches: list[dict], launcher: str) -> dict:
    """Aggregate multiple batches into single status."""
    total_expected = sum(b['expected'] for b in batches)
    total_ready = sum(b['ready'] for b in batches)
    all_instances = []
    for b in batches:
        all_instances.extend(b['instances'])

    return {
        'expected': total_expected,
        'ready': total_ready,
        'instances': all_instances,
        'batches': [b['batch_id'] for b in batches],
        'launcher': launcher,
        'timestamp': batches[0]['timestamp'],  # most recent
    }


def get_launch_batch(batch_id: str) -> dict | None:
    """Get single batch status by ID prefix."""
    conn = get_db()

    row = conn.execute("""
        SELECT e.timestamp, e.instance as launcher,
               json_extract(e.data, '$.batch_id') as batch_id,
               json_extract(e.data, '$.launched') as expected
        FROM events e
        WHERE e.type = 'life'
          AND json_extract(e.data, '$.action') = 'launched'
          AND json_extract(e.data, '$.batch_id') LIKE ?
        ORDER BY e.id DESC LIMIT 1
    """, (f'{batch_id}%',)).fetchone()

    if not row:
        return None

    ready_rows = conn.execute("""
        SELECT instance FROM events
        WHERE type = 'life'
          AND json_extract(data, '$.action') = 'ready'
          AND json_extract(data, '$.batch_id') = ?
    """, (row['batch_id'],)).fetchall()

    return {
        'batch_id': row['batch_id'],
        'expected': row['expected'] or 0,
        'ready': len(ready_rows),
        'instances': [r['instance'] for r in ready_rows],
        'launcher': row['launcher'],
        'timestamp': row['timestamp'],
    }

# ==================== KV Store ====================

def kv_get(key: str) -> str | None:
    """Get value from kv table."""
    conn = get_db()
    row = conn.execute("SELECT value FROM kv WHERE key = ?", (key,)).fetchone()
    return row['value'] if row else None

def kv_set(key: str, value: str | None) -> None:
    """Set or delete value in kv table."""
    conn = get_db()
    with _write_lock:
        if value is None:
            conn.execute("DELETE FROM kv WHERE key = ?", (key,))
        else:
            conn.execute(
                "INSERT OR REPLACE INTO kv (key, value) VALUES (?, ?)",
                (key, value)
            )
        conn.commit()

__all__ = [
    # Events
    'get_db',
    'close_db',
    'init_db',
    'log_event',
    'get_events_since',
    'get_last_event_id',
    'DB_FILE',
    # Instances (low-level)
    'get_instance',
    'save_instance',
    'update_instance',
    'find_instance_by_session',
    'get_instance_by_mapid',
    # Instances (high-level queries)
    'iter_instances',
    # Launch batch
    'LAUNCH_TIMEOUT_SECONDS',
    'get_launch_status',
    'get_launch_batch',
    # KV store
    'kv_get',
    'kv_set',
]