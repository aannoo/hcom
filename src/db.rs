//! SQLite database access for hcom
//!
//! Three loosely-coupled state planes live in a single DB:
//! - `instances`: live per-agent state (TUI display, gating, delivery cursors)
//! - `events`: append-only history / message log / relay replication source
//! - `process_bindings`, `session_bindings`, `notify_endpoints`, `kv`: routing
//!   and control-plane state
//!
//! Callers typically write an event, advance per-instance cursors separately,
//! and touch bindings/endpoints/kv for delivery, identity resolution, relay
//! cursors, request-watch bookkeeping, and other control-plane state.
//!
//! Includes:
//! - Reading unread messages from `events`
//! - Updating cursor position (instances.last_event_id)
//! - Reading instance status
//! - Registering notify endpoints

use anyhow::{Context, Result, bail};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, params};

pub(crate) mod subscriptions;

/// Schema version - bump on any schema change.
const SCHEMA_VERSION: i32 = 17;
pub const DEV_ROOT_KV_KEY: &str = "config:dev_root";
const MIGRATIONS: &[(i32, &str)] = &[(
    17,
    "ALTER TABLE instances ADD COLUMN terminal_preset_requested TEXT DEFAULT '';
     ALTER TABLE instances ADD COLUMN terminal_preset_effective TEXT DEFAULT '';
     UPDATE instances
     SET terminal_preset_effective = json_extract(launch_context, '$.terminal_preset')
     WHERE launch_context != '' AND json_valid(launch_context) AND json_extract(launch_context, '$.terminal_preset') IS NOT NULL;",
)];

use crate::shared::constants::ST_LISTENING;
use crate::shared::time::{now_epoch_f64, now_epoch_i64};

/// Message from the events table
#[derive(Debug, Clone)]
pub struct Message {
    pub from: String,
    pub text: String,
    pub intent: Option<String>,
    pub thread: Option<String>,
    pub event_id: Option<i64>,
    pub timestamp: Option<String>,
    pub delivered_to: Option<Vec<String>>,
    pub bundle_id: Option<String>,
    pub relay: bool,
}

/// Instance status info
#[derive(Debug, Clone, PartialEq)]
pub struct InstanceStatus {
    pub status: String,
    pub detail: String,
    pub last_event_id: i64,
}

/// Schema compatibility check result
enum SchemaCompat {
    /// Schema is compatible (or fresh DB) — proceed with init_db
    Ok,
    /// Schema is incompatible — archive, reconnect, reinit
    NeedsArchive(String, Option<i32>),
    /// DB is newer than code — stale process, work with existing schema
    StaleProcess,
}

/// Full instance row from the instances table.
#[derive(Debug, Clone)]
pub struct InstanceRow {
    pub name: String,
    pub session_id: Option<String>,
    pub parent_session_id: Option<String>,
    pub parent_name: Option<String>,
    pub agent_id: Option<String>,
    pub tag: Option<String>,
    pub last_event_id: i64,
    pub last_stop: i64,
    pub status: String,
    pub status_time: i64,
    pub status_context: String,
    pub status_detail: String,
    pub directory: String,
    pub created_at: f64,
    pub transcript_path: String,
    pub tool: String,
    pub background: i64,
    pub background_log_file: String,
    pub tcp_mode: i64,
    pub wait_timeout: Option<i64>,
    pub subagent_timeout: Option<i64>,
    pub hints: Option<String>,
    pub origin_device_id: Option<String>,
    pub pid: Option<i64>,
    pub launch_args: Option<String>,
    pub terminal_preset_requested: Option<String>,
    pub terminal_preset_effective: Option<String>,
    pub launch_context: Option<String>,
    pub name_announced: i64,
    pub running_tasks: Option<String>,
    pub idle_since: Option<String>,
}

impl InstanceRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            name: row.get("name")?,
            session_id: row
                .get::<_, Option<String>>("session_id")?
                .filter(|s| !s.is_empty()),
            parent_session_id: row
                .get::<_, Option<String>>("parent_session_id")?
                .filter(|s| !s.is_empty()),
            parent_name: row
                .get::<_, Option<String>>("parent_name")?
                .filter(|s| !s.is_empty()),
            agent_id: row
                .get::<_, Option<String>>("agent_id")?
                .filter(|s| !s.is_empty()),
            tag: row
                .get::<_, Option<String>>("tag")?
                .filter(|s| !s.is_empty()),
            last_event_id: row.get::<_, Option<i64>>("last_event_id")?.unwrap_or(0),
            last_stop: row.get::<_, Option<i64>>("last_stop")?.unwrap_or(0),
            status: row
                .get::<_, Option<String>>("status")?
                .unwrap_or_else(|| "inactive".into()),
            status_time: row.get::<_, Option<i64>>("status_time")?.unwrap_or(0),
            status_context: row
                .get::<_, Option<String>>("status_context")?
                .unwrap_or_default(),
            status_detail: row
                .get::<_, Option<String>>("status_detail")?
                .unwrap_or_default(),
            directory: row
                .get::<_, Option<String>>("directory")?
                .unwrap_or_default(),
            created_at: row.get::<_, Option<f64>>("created_at")?.unwrap_or(0.0),
            transcript_path: row
                .get::<_, Option<String>>("transcript_path")?
                .unwrap_or_default(),
            tool: row
                .get::<_, Option<String>>("tool")?
                .unwrap_or_else(|| "claude".into()),
            background: row.get::<_, Option<i64>>("background")?.unwrap_or(0),
            background_log_file: row
                .get::<_, Option<String>>("background_log_file")?
                .unwrap_or_default(),
            tcp_mode: row.get::<_, Option<i64>>("tcp_mode")?.unwrap_or(0),
            wait_timeout: row.get::<_, Option<i64>>("wait_timeout")?,
            subagent_timeout: row.get::<_, Option<i64>>("subagent_timeout")?,
            hints: row
                .get::<_, Option<String>>("hints")?
                .filter(|s| !s.is_empty()),
            origin_device_id: row
                .get::<_, Option<String>>("origin_device_id")?
                .filter(|s| !s.is_empty()),
            pid: row.get::<_, Option<i64>>("pid")?,
            launch_args: row
                .get::<_, Option<String>>("launch_args")?
                .filter(|s| !s.is_empty()),
            terminal_preset_requested: row
                .get::<_, Option<String>>("terminal_preset_requested")?
                .filter(|s| !s.is_empty()),
            terminal_preset_effective: row
                .get::<_, Option<String>>("terminal_preset_effective")?
                .filter(|s| !s.is_empty()),
            launch_context: row
                .get::<_, Option<String>>("launch_context")?
                .filter(|s| !s.is_empty()),
            name_announced: row.get::<_, Option<i64>>("name_announced")?.unwrap_or(0),
            running_tasks: row
                .get::<_, Option<String>>("running_tasks")?
                .filter(|s| !s.is_empty()),
            idle_since: row
                .get::<_, Option<String>>("idle_since")?
                .filter(|s| !s.is_empty()),
        })
    }
}

/// Database handle for hcom operations
pub struct HcomDb {
    conn: Connection,
    db_path: std::path::PathBuf,
    db_inode: u64,
}

fn get_inode(path: &std::path::Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).map(|m| m.ino()).unwrap_or(0)
}

impl HcomDb {
    /// Access the underlying SQLite connection (for direct queries).
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Access the filesystem path backing this DB handle.
    pub fn path(&self) -> &std::path::Path {
        &self.db_path
    }

    /// Open the hcom database at ~/.hcom/hcom.db with schema migration/compat.
    pub fn open() -> Result<Self> {
        let db_path = crate::paths::db_path();
        Self::open_at(&db_path)
    }

    /// Open the hcom database at a specific path with schema migration/compat.
    pub fn open_at(db_path: &std::path::Path) -> Result<Self> {
        let mut db = Self::open_raw(db_path)?;
        db.ensure_schema()?;
        Ok(db)
    }

    /// Open DB connection without schema checks (for testing only).
    pub fn open_raw(db_path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create db directory: {}", parent.display()))?;
        }
        let conn = Connection::open(db_path)
            .with_context(|| format!("Failed to open database: {}", db_path.display()))?;

        // Enable WAL mode for concurrent access + foreign keys for CASCADE
        conn.execute_batch(
            "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;",
        )?;

        let inode = get_inode(db_path);

        Ok(Self {
            conn,
            db_path: db_path.to_path_buf(),
            db_inode: inode,
        })
    }

    /// Reconnect if the DB file was replaced (e.g., by hcom reset / schema bump).
    /// Long-lived threads (PTY delivery, listeners) hold an open connection to the
    /// old inode; this moves them onto the new DB file.
    /// Returns true if reconnection happened.
    pub fn reconnect_if_stale(&mut self) -> bool {
        let current_inode = get_inode(&self.db_path);
        if current_inode == 0 || current_inode == self.db_inode {
            return false;
        }
        // DB file replaced — reconnect
        use crate::log::{log_error, log_info};
        match Connection::open(&self.db_path) {
            Ok(new_conn) => {
                if let Err(e) = new_conn.execute_batch(
                    "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;",
                ) {
                    use crate::log::log_warn;
                    log_warn(
                        "native",
                        "db.pragma_fail",
                        &format!("PRAGMA setup failed after reconnect: {}", e),
                    );
                }
                log_info(
                    "native",
                    "db.reconnect",
                    &format!(
                        "DB file replaced (inode {} -> {}), reconnected",
                        self.db_inode, current_inode
                    ),
                );
                self.conn = new_conn;
                self.db_inode = current_inode;
                true
            }
            Err(e) => {
                log_error(
                    "native",
                    "db.reconnect_fail",
                    &format!("Failed to reconnect: {}", e),
                );
                false
            }
        }
    }

    /// Initialize database schema. Idempotent (IF NOT EXISTS).
    /// Creates all tables, indexes, events_v view, FTS5 virtual table + trigger,
    /// and sets PRAGMA user_version.
    pub fn init_db(&self) -> Result<()> {
        // Skip if already at current version (avoids DROP VIEW race with concurrent readers)
        let current: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if current == SCHEMA_VERSION {
            return Ok(());
        }

        self.conn.execute_batch(
            "
            -- Events table
            CREATE TABLE IF NOT EXISTS events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                type TEXT NOT NULL,
                instance TEXT NOT NULL,
                data TEXT NOT NULL
            );

            -- Notify endpoints
            CREATE TABLE IF NOT EXISTS notify_endpoints (
                instance TEXT NOT NULL,
                kind TEXT NOT NULL,
                port INTEGER NOT NULL,
                updated_at REAL NOT NULL,
                PRIMARY KEY (instance, kind)
            );
            CREATE INDEX IF NOT EXISTS idx_notify_endpoints_instance ON notify_endpoints(instance);
            CREATE INDEX IF NOT EXISTS idx_notify_endpoints_port ON notify_endpoints(port);

            -- Process bindings
            CREATE TABLE IF NOT EXISTS process_bindings (
                process_id TEXT PRIMARY KEY,
                session_id TEXT,
                instance_name TEXT,
                updated_at REAL NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_process_bindings_instance ON process_bindings(instance_name);
            CREATE INDEX IF NOT EXISTS idx_process_bindings_session ON process_bindings(session_id);

            -- Session bindings
            CREATE TABLE IF NOT EXISTS session_bindings (
                session_id TEXT PRIMARY KEY,
                instance_name TEXT NOT NULL,
                created_at REAL NOT NULL,
                FOREIGN KEY (instance_name) REFERENCES instances(name) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_session_bindings_instance ON session_bindings(instance_name);

            -- Instances table
            CREATE TABLE IF NOT EXISTS instances (
                name TEXT PRIMARY KEY,
                session_id TEXT UNIQUE,
                parent_session_id TEXT,
                parent_name TEXT,
                tag TEXT,
                last_event_id INTEGER DEFAULT 0,
                status TEXT DEFAULT 'active',
                status_time INTEGER DEFAULT 0,
                status_context TEXT DEFAULT '',
                status_detail TEXT DEFAULT '',
                last_stop INTEGER DEFAULT 0,
                directory TEXT,
                created_at REAL NOT NULL,
                transcript_path TEXT DEFAULT '',
                tcp_mode INTEGER DEFAULT 0,
                wait_timeout INTEGER DEFAULT 86400,
                background INTEGER DEFAULT 0,
                background_log_file TEXT DEFAULT '',
                name_announced INTEGER DEFAULT 0,
                agent_id TEXT UNIQUE,
                running_tasks TEXT DEFAULT '',
                origin_device_id TEXT DEFAULT '',
                hints TEXT DEFAULT '',
                subagent_timeout INTEGER,
                tool TEXT DEFAULT 'claude',
                launch_args TEXT DEFAULT '',
                terminal_preset_requested TEXT DEFAULT '',
                terminal_preset_effective TEXT DEFAULT '',
                idle_since TEXT DEFAULT '',
                pid INTEGER DEFAULT NULL,
                launch_context TEXT DEFAULT '',
                FOREIGN KEY (parent_session_id) REFERENCES instances(session_id) ON DELETE SET NULL
            );

            -- KV table
            CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT);

            -- Event indexes
            CREATE INDEX IF NOT EXISTS idx_timestamp ON events(timestamp);
            CREATE INDEX IF NOT EXISTS idx_type ON events(type);
            CREATE INDEX IF NOT EXISTS idx_instance ON events(instance);
            CREATE INDEX IF NOT EXISTS idx_type_instance ON events(type, instance);

            -- Instance indexes
            CREATE INDEX IF NOT EXISTS idx_session_id ON instances(session_id);
            CREATE INDEX IF NOT EXISTS idx_parent_session_id ON instances(parent_session_id);
            CREATE INDEX IF NOT EXISTS idx_parent_name ON instances(parent_name);
            CREATE INDEX IF NOT EXISTS idx_created_at ON instances(created_at DESC);
            CREATE INDEX IF NOT EXISTS idx_status ON instances(status);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_id_unique ON instances(agent_id) WHERE agent_id IS NOT NULL;
            CREATE INDEX IF NOT EXISTS idx_instances_origin ON instances(origin_device_id);

            -- Flattened events view (DROP first to apply schema changes)
            DROP VIEW IF EXISTS events_v;
            CREATE VIEW IF NOT EXISTS events_v AS
            SELECT
                id, timestamp, type, instance, data,
                json_extract(data, '$.from') as msg_from,
                json_extract(data, '$.text') as msg_text,
                json_extract(data, '$.scope') as msg_scope,
                json_extract(data, '$.sender_kind') as msg_sender_kind,
                json_extract(data, '$.delivered_to') as msg_delivered_to,
                json_extract(data, '$.mentions') as msg_mentions,
                json_extract(data, '$.intent') as msg_intent,
                json_extract(data, '$.thread') as msg_thread,
                json_extract(data, '$.reply_to') as msg_reply_to,
                json_extract(data, '$.reply_to_local') as msg_reply_to_local,
                json_extract(data, '$.bundle_id') as bundle_id,
                json_extract(data, '$.title') as bundle_title,
                json_extract(data, '$.description') as bundle_description,
                json_extract(data, '$.extends') as bundle_extends,
                json_extract(data, '$.refs.events') as bundle_events,
                json_extract(data, '$.refs.files') as bundle_files,
                json_extract(data, '$.refs.transcript') as bundle_transcript,
                json_extract(data, '$.created_by') as bundle_created_by,
                json_extract(data, '$.status') as status_val,
                json_extract(data, '$.context') as status_context,
                json_extract(data, '$.detail') as status_detail,
                json_extract(data, '$.action') as life_action,
                json_extract(data, '$.by') as life_by,
                json_extract(data, '$.batch_id') as life_batch_id,
                json_extract(data, '$.reason') as life_reason
            FROM events;

            -- FTS5 full-text search index
            CREATE VIRTUAL TABLE IF NOT EXISTS events_fts USING fts5(
                searchable,
                tokenize='unicode61'
            );
            CREATE TRIGGER IF NOT EXISTS events_fts_insert
            AFTER INSERT ON events BEGIN
                INSERT INTO events_fts(rowid, searchable) VALUES (
                    new.id,
                    COALESCE(json_extract(new.data, '$.text'), '') || ' ' ||
                    COALESCE(json_extract(new.data, '$.from'), '') || ' ' ||
                    COALESCE(new.instance, '') || ' ' ||
                    COALESCE(json_extract(new.data, '$.context'), '') || ' ' ||
                    COALESCE(json_extract(new.data, '$.detail'), '') || ' ' ||
                    COALESCE(json_extract(new.data, '$.action'), '') || ' ' ||
                    COALESCE(json_extract(new.data, '$.reason'), '')
                );
            END;
            ",
        )?;

        // Set schema version
        self.conn
            .execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION))?;

        Ok(())
    }

    /// Full schema bootstrap: check version, archive if mismatched, reconnect, init.
    ///
    /// Checks schema version, archives DB if mismatched, reconnects, and reinitializes.
    /// Call after open() for production use.
    pub fn ensure_schema(&mut self) -> Result<()> {
        match self.check_schema_compat()? {
            SchemaCompat::Ok => {
                self.init_db()?;
                Ok(())
            }
            SchemaCompat::NeedsArchive(reason, old_version) => {
                if let Some(version) = old_version {
                    // If version matches but columns are missing (stamped without migration),
                    // repair by running migrations from version-1.
                    let migrate_from = if version == SCHEMA_VERSION {
                        version - 1
                    } else {
                        version
                    };
                    match self.try_apply_migrations(migrate_from) {
                        Ok(true) => return Ok(()),
                        Ok(false) => {}
                        Err(e) => {
                            crate::log::log_warn(
                                "db",
                                "schema.migration_failed",
                                &format!("v{} -> v{} failed: {}", migrate_from, SCHEMA_VERSION, e),
                            );
                        }
                    }
                }
                eprintln!("hcom: {}, archiving...", reason);

                // Snapshot running instances to pidtrack before archive so orphan
                // recovery can re-register them into the fresh DB.
                self.snapshot_running_to_pidtrack();

                // Archive the old DB
                let archive_path = Self::archive_db_at(&self.db_path)?;
                if let Some(ref path) = archive_path {
                    eprintln!("hcom: Archived to {}", path);
                    eprintln!("       Query with: hcom archive 1");
                }

                // Reconnect to fresh DB file
                let new_conn = Connection::open(&self.db_path).with_context(|| {
                    format!(
                        "Failed to reopen DB after archive: {}",
                        self.db_path.display()
                    )
                })?;
                new_conn.execute_batch(
                    "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;",
                )?;
                self.conn = new_conn;
                self.db_inode = get_inode(&self.db_path);

                // Init fresh schema
                self.init_db()?;

                // Log reset event to fresh DB
                self.log_reset_event()?;

                Ok(())
            }
            SchemaCompat::StaleProcess => {
                // DB is newer than our code — work with it, don't archive
                Ok(())
            }
        }
    }

    /// Internal: check schema compatibility without taking action.
    fn check_schema_compat(&self) -> Result<SchemaCompat> {
        let version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap_or(0);

        // Check what tables exist
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")?;
        let tables: std::collections::HashSet<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();

        let required: std::collections::HashSet<&str> = [
            "events",
            "instances",
            "kv",
            "notify_endpoints",
            "session_bindings",
        ]
        .into_iter()
        .collect();

        if version == 0 {
            // Race handling: another process may be initializing
            if !tables.is_empty() && required.iter().any(|t| tables.contains(*t)) {
                let mut resolved_version = 0i32;
                for _ in 0..20 {
                    let v2: i32 = self
                        .conn
                        .query_row("PRAGMA user_version", [], |row| row.get(0))
                        .unwrap_or(0);
                    if v2 != 0 {
                        resolved_version = v2;
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                if resolved_version == SCHEMA_VERSION {
                    return Ok(SchemaCompat::Ok);
                }
                if resolved_version > SCHEMA_VERSION {
                    crate::log::log_warn(
                        "db",
                        "schema.stale_process",
                        &format!(
                            "DB v{} > code v{}, working with newer schema",
                            resolved_version, SCHEMA_VERSION
                        ),
                    );
                    return Ok(SchemaCompat::StaleProcess);
                }
                // Timeout exhausted — another process is still initializing.
                // Return Ok rather than falling through to NeedsArchive which
                // would incorrectly archive a valid in-progress DB.
                if resolved_version == 0 {
                    crate::log::log_warn(
                        "db",
                        "schema.init_timeout",
                        "Concurrent init poll timed out, assuming OK",
                    );
                    return Ok(SchemaCompat::Ok);
                }
            }
            // Fresh DB (no tables) - safe to initialize
            if tables.is_empty() {
                return Ok(SchemaCompat::Ok);
            }
            // Pre-versioned DB with our tables - needs archive
            if required.iter().any(|t| tables.contains(*t)) {
                return Ok(SchemaCompat::NeedsArchive(
                    "Pre-versioned DB found".to_string(),
                    None,
                ));
            }
            // Has tables but not ours - fresh enough
            return Ok(SchemaCompat::Ok);
        }

        if version != SCHEMA_VERSION {
            if version > SCHEMA_VERSION {
                // DB newer than code - stale process, work with it
                crate::log::log_warn(
                    "db",
                    "schema.stale_process",
                    &format!(
                        "DB v{} > code v{}, working with newer schema",
                        version, SCHEMA_VERSION
                    ),
                );
                return Ok(SchemaCompat::StaleProcess);
            }
            // DB older - needs archive
            return Ok(SchemaCompat::NeedsArchive(
                format!(
                    "DB version mismatch (DB v{}, code v{})",
                    version, SCHEMA_VERSION
                ),
                Some(version),
            ));
        }

        // Verify required tables exist
        let have_all = required.iter().all(|t| tables.contains(*t));
        if !have_all {
            let missing: Vec<&&str> = required.iter().filter(|t| !tables.contains(**t)).collect();
            return Ok(SchemaCompat::NeedsArchive(
                format!("DB missing tables {:?}", missing),
                None,
            ));
        }

        // Column guard: verify all expected columns exist (catches partial schema from
        // version bump before migration was written)
        let missing_col: Option<String> = self
            .conn
            .prepare("PRAGMA table_info(instances)")
            .and_then(|mut s| {
                let cols: Vec<String> = s
                    .query_map([], |row| row.get::<_, String>(1))?
                    .filter_map(|r| r.ok())
                    .collect();
                let required = [
                    "tool",
                    "terminal_preset_requested",
                    "terminal_preset_effective",
                ];
                Ok(required
                    .iter()
                    .find(|c| !cols.contains(&c.to_string()))
                    .map(|s| s.to_string()))
            })
            .unwrap_or(None);
        if let Some(col) = missing_col {
            return Ok(SchemaCompat::NeedsArchive(
                format!("DB schema missing instances.{}", col),
                Some(version),
            ));
        }

        Ok(SchemaCompat::Ok)
    }

    /// Try in-place migration for consecutive schema versions.
    ///
    /// Returns `Ok(false)` if any step is missing from `MIGRATIONS`,
    /// causing `ensure_schema()` to fall back to archive+recreate.
    fn try_apply_migrations(&self, old_version: i32) -> Result<bool> {
        if old_version <= 0 || old_version >= SCHEMA_VERSION {
            return Ok(false);
        }
        let tx = self.conn.unchecked_transaction()?;
        for next_version in (old_version + 1)..=SCHEMA_VERSION {
            let Some((_, sql)) = MIGRATIONS.iter().find(|(v, _)| *v == next_version) else {
                return Ok(false);
            };
            tx.execute_batch(sql)?;
            tx.execute_batch(&format!("PRAGMA user_version = {}", next_version))?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// Archive current database at a given path.
    /// WAL checkpoint, copy to archive dir (sibling archive/ directory), delete original.
    fn archive_db_at(db_path: &std::path::Path) -> Result<Option<String>> {
        if !db_path.exists() {
            return Ok(None);
        }

        let db_wal = db_path.with_extension("db-wal");
        let db_shm = db_path.with_extension("db-shm");

        // WAL checkpoint before archive
        if let Ok(temp_conn) = Connection::open(db_path) {
            let _ = temp_conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
        }

        // Create archive directory next to the DB file
        let parent = db_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let timestamp = Utc::now().format("%Y-%m-%d_%H%M%S").to_string();
        let archive_dir = parent
            .join("archive")
            .join(format!("session-{}", timestamp));
        std::fs::create_dir_all(&archive_dir)?;

        // Copy DB files to archive
        let db_name = db_path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("hcom.db"));
        std::fs::copy(db_path, archive_dir.join(db_name))?;
        if db_wal.exists() {
            let wal_name = format!("{}-wal", db_name.to_string_lossy());
            let _ = std::fs::copy(&db_wal, archive_dir.join(wal_name));
        }
        if db_shm.exists() {
            let shm_name = format!("{}-shm", db_name.to_string_lossy());
            let _ = std::fs::copy(&db_shm, archive_dir.join(shm_name));
        }

        // Delete original
        std::fs::remove_file(db_path)?;
        let _ = std::fs::remove_file(&db_wal);
        let _ = std::fs::remove_file(&db_shm);

        Ok(Some(archive_dir.to_string_lossy().to_string()))
    }

    /// Snapshot running instances to pidtrack before DB archive.
    ///
    /// Writes live instances (with their PIDs) to ~/.hcom/.tmp/launched_pids.json
    /// so orphan recovery can re-register them into the fresh DB after schema bump.
    fn snapshot_running_to_pidtrack(&self) {
        let Ok(mut stmt) = self.conn.prepare(
            "SELECT i.name, i.pid, i.tool, i.directory, i.session_id, p.process_id, \
                    n_pty.port AS notify_port, n_inj.port AS inject_port \
             FROM instances i \
             LEFT JOIN process_bindings p ON i.name = p.instance_name \
             LEFT JOIN notify_endpoints n_pty ON i.name = n_pty.instance AND n_pty.kind = 'pty' \
             LEFT JOIN notify_endpoints n_inj ON i.name = n_inj.instance AND n_inj.kind = 'inject' \
             WHERE i.pid IS NOT NULL",
        ) else {
            return;
        };

        let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,         // name
                row.get::<_, i64>(1)?,            // pid
                row.get::<_, Option<String>>(2)?, // tool
                row.get::<_, Option<String>>(3)?, // directory
                row.get::<_, Option<String>>(4)?, // session_id
                row.get::<_, Option<String>>(5)?, // process_id
                row.get::<_, Option<i64>>(6)?,    // notify_port
                row.get::<_, Option<i64>>(7)?,    // inject_port
            ))
        }) else {
            return;
        };

        let pidfile_path = self
            .db_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(".tmp")
            .join("launched_pids.json");

        // Read existing pidfile
        let mut piddata: serde_json::Map<String, serde_json::Value> =
            std::fs::read_to_string(&pidfile_path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();

        for row in rows.flatten() {
            let (name, pid, tool, directory, session_id, process_id, notify_port, inject_port) =
                row;
            let alive = crate::pidtrack::is_alive(pid as u32);
            if !alive {
                continue;
            }

            piddata.insert(
                pid.to_string(),
                serde_json::json!({
                    "tool": tool.unwrap_or_else(|| "claude".to_string()),
                    "names": [name],
                    "directory": directory.unwrap_or_default(),
                    "process_id": process_id.unwrap_or_default(),
                    "session_id": session_id.unwrap_or_default(),
                    "notify_port": notify_port.unwrap_or(0),
                    "inject_port": inject_port.unwrap_or(0),
                    "launched_at": now_epoch_f64(),
                }),
            );
        }

        if let Ok(json) = serde_json::to_string(&piddata) {
            let _ = std::fs::write(&pidfile_path, json);
        }
    }

    /// Log _device reset event + set relay timestamp. Call after any DB archive/reset.
    pub fn log_reset_event(&self) -> Result<()> {
        // Derive hcom_dir from db_path (db is at hcom_dir/hcom.db)
        let hcom_dir = self
            .db_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let device_id = std::fs::read_to_string(hcom_dir.join(".tmp").join("device_uuid"))
            .unwrap_or_else(|_| "unknown".to_string())
            .trim()
            .to_string();

        self.log_event(
            "life",
            "_device",
            &serde_json::json!({"action": "reset", "device": device_id}),
        )?;

        self.kv_set("relay_local_reset_ts", Some(&now_epoch_f64().to_string()))?;

        Ok(())
    }

    /// Get instance status by name
    ///
    /// Returns:
    /// - Ok(Some(status)) if instance exists
    /// - Ok(None) if instance not found
    /// - Err if database error occurs
    pub fn get_instance_status(&self, name: &str) -> Result<Option<InstanceStatus>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT name, status, status_detail, last_event_id
             FROM instances WHERE name = ?",
        )?;

        match stmt.query_row(params![name], |row| {
            Ok(InstanceStatus {
                status: row
                    .get::<_, String>(1)
                    .unwrap_or_else(|_| "unknown".to_string()),
                detail: row.get::<_, String>(2).unwrap_or_default(),
                last_event_id: row.get::<_, i64>(3).unwrap_or(0),
            })
        }) {
            Ok(status) => Ok(Some(status)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Check if a message event should be delivered to the given receiver.
    ///
    /// Skips own messages. Checks scope: "broadcast" delivers to all,
    /// "mentions" checks the mentions array with cross-device base-name matching.
    ///
    /// `receiver` may be local (`luna`) or relay-namespaced (`luna:ABCD`).
    /// Mentions compare on base name so the same event JSON routes correctly
    /// on both local and relayed peers without rewriting stored scope.
    fn should_deliver_to(json: &serde_json::Value, receiver: &str) -> bool {
        let from = json.get("from").and_then(|v| v.as_str()).unwrap_or("");
        if from == receiver {
            return false;
        }
        let scope = json
            .get("scope")
            .and_then(|s| s.as_str())
            .unwrap_or("broadcast");
        match scope {
            "broadcast" => true,
            "mentions" => {
                let receiver_base = receiver.split(':').next().unwrap_or(receiver);
                json.get("mentions")
                    .and_then(|m| m.as_array())
                    .map(|arr| {
                        arr.iter().any(|v| {
                            v.as_str()
                                .map(|m| receiver_base == m.split(':').next().unwrap_or(m))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            }
            _ => false,
        }
    }

    /// Returns true iff there is at least one unread message that names this
    /// instance directly (`scope='mentions'` and the recipient is in the
    /// `mentions` array). Broadcasts are ignored.
    ///
    /// Used to gate dormant subagent activation: a SubagentStart-allocated
    /// row is in the broadcast recipient set, but we don't want a passing
    /// broadcast to wake a subagent nobody actually addressed.
    pub fn has_direct_unread(&self, name: &str) -> bool {
        let last_event_id = match self.get_instance_status(name) {
            Ok(Some(status)) => status.last_event_id,
            _ => 0,
        };
        let mut stmt = match self.conn.prepare_cached(
            "SELECT data FROM events
             WHERE id > ? AND type = 'message'
             ORDER BY id",
        ) {
            Ok(s) => s,
            Err(_) => return false,
        };
        let rows = match stmt.query_map(params![last_event_id], |row| row.get::<_, String>(0)) {
            Ok(r) => r,
            Err(_) => return false,
        };
        for data in rows.flatten() {
            let Ok(json) = serde_json::from_str::<serde_json::Value>(&data) else {
                continue;
            };
            let scope = json
                .get("scope")
                .and_then(|s| s.as_str())
                .unwrap_or("broadcast");
            if scope != "mentions" {
                continue;
            }
            if Self::should_deliver_to(&json, name) {
                return true;
            }
        }
        false
    }

    /// Get unread messages for an instance
    ///
    /// Returns messages where:
    /// - event.id > instance.last_event_id
    /// - event.type = 'message'
    /// - instance is in scope (broadcast or direct)
    pub fn get_unread_messages(&self, name: &str) -> Vec<Message> {
        // Get last_event_id for this instance
        let last_event_id = match self.get_instance_status(name) {
            Ok(Some(status)) => status.last_event_id,
            Ok(None) => 0, // No instance found
            Err(e) => {
                crate::log::log_error(
                    "db",
                    "get_unread_messages.get_instance_status",
                    &format!("{e}"),
                );
                0
            }
        };

        let mut stmt = match self.conn.prepare_cached(
            "SELECT id, timestamp, data FROM events
             WHERE id > ? AND type = 'message'
             ORDER BY id",
        ) {
            Ok(s) => s,
            Err(e) => {
                crate::log::log_error("db", "get_unread_messages.prepare", &format!("{e}"));
                return vec![];
            }
        };

        let rows = match stmt.query_map(params![last_event_id], |row| {
            let id: i64 = row.get(0)?;
            let timestamp: String = row.get(1)?;
            let data: String = row.get(2)?;
            Ok((id, timestamp, data))
        }) {
            Ok(r) => r,
            Err(e) => {
                crate::log::log_error("db", "get_unread_messages.query", &format!("{e}"));
                return vec![];
            }
        };

        let mut messages = Vec::new();
        for (id, timestamp, data) in rows.flatten() {
            // Parse JSON data
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&data) {
                if !Self::should_deliver_to(&json, name) {
                    continue;
                }

                let from = json
                    .get("from")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();

                let text = json
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let intent = json
                    .get("intent")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let thread = json
                    .get("thread")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let delivered_to = json
                    .get("delivered_to")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    });
                let bundle_id = json
                    .get("bundle_id")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let relay = json
                    .get("_relay")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                messages.push(Message {
                    from,
                    text,
                    intent,
                    thread,
                    event_id: Some(id),
                    timestamp: Some(timestamp.clone()),
                    delivered_to,
                    bundle_id,
                    relay,
                });
            }
        }

        messages
    }

    /// Register notify endpoint for PTY wake-ups
    ///
    /// Inserts or updates notify_endpoints table with (instance, kind='pty', port)
    pub fn register_notify_port(&self, name: &str, port: u16) -> Result<()> {
        self.upsert_notify_endpoint(name, "pty", port)
    }

    /// Register inject port for screen queries
    pub fn register_inject_port(&self, name: &str, port: u16) -> Result<()> {
        self.upsert_notify_endpoint(name, "inject", port)
    }

    /// Check if instance is idle (safe for PTY injection).
    /// Returns true only when status is "listening" AND detail is not "cmd:listen".
    /// The "cmd:listen" detail is set by `hcom listen` as its first operation,
    /// ensuring the gate blocks before any async setup (endpoint registration, etc.).
    pub fn is_idle(&self, name: &str) -> bool {
        match self.get_instance_status(name) {
            Ok(Some(s)) => s.status == ST_LISTENING && s.detail != "cmd:listen",
            _ => false,
        }
    }

    /// Update heartbeat timestamp and re-assert tcp_mode to prove instance is alive.
    ///
    /// Sets both last_stop (heartbeat) and tcp_mode=true atomically.
    /// Re-asserting tcp_mode on every heartbeat self-heals after DB resets,
    /// instance re-creation, or any state loss — the delivery thread is the
    /// source of truth for whether TCP delivery is active.
    pub fn update_heartbeat(&self, name: &str) -> Result<()> {
        let now = now_epoch_i64();

        self.conn.execute(
            "UPDATE instances SET last_stop = ?, tcp_mode = 1 WHERE name = ?",
            params![now, name],
        )?;
        Ok(())
    }

    /// Update instance position with tcp_mode flag
    pub fn update_tcp_mode(&self, name: &str, tcp_mode: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE instances SET tcp_mode = ? WHERE name = ?",
            params![tcp_mode as i32, name],
        )?;
        Ok(())
    }

    /// Set instance status in the live `instances` row.
    ///
    /// Side effect: the first call after instance creation (`status_context == "new"`)
    /// emits a `life` event with `action: "ready"` and may trigger batch-completion
    /// notification. For transient TUI-only context, use `set_gate_status()` instead.
    pub fn set_status(&self, name: &str, status: &str, context: &str) -> Result<()> {
        // Check if this is first status update (status_context="new" → ready event)
        let is_new = self
            .get_status(name)?
            .map(|(_, ctx)| ctx == "new")
            .unwrap_or(false);

        let now = now_epoch_i64();

        // Update last_stop heartbeat when entering listening state
        if status == ST_LISTENING {
            self.conn.execute(
                "UPDATE instances SET status = ?, status_context = ?, status_time = ?, last_stop = ? WHERE name = ?",
                params![status, context, now, now, name],
            )?;
        } else {
            self.conn.execute(
                "UPDATE instances SET status = ?, status_context = ?, status_time = ? WHERE name = ?",
                params![status, context, now, name],
            )?;
        }

        // Emit ready event and batch notification on first status update
        if is_new {
            if let Err(e) = self.emit_ready_event(name, status, context) {
                crate::log::log_error("db", "set_status.emit_ready_event", &format!("{e}"));
            }
        }

        Ok(())
    }

    /// Emit "ready" life event and check for batch completion notification.
    ///
    /// Called on first status update (when status_context was "new").
    fn emit_ready_event(&self, name: &str, status: &str, context: &str) -> Result<()> {
        let launcher = std::env::var("HCOM_LAUNCHED_BY").unwrap_or_else(|_| "unknown".to_string());
        let batch_id = std::env::var("HCOM_LAUNCH_BATCH_ID").ok();

        let mut event_data = serde_json::json!({
            "action": "ready",
            "by": launcher,
            "status": status,
            "context": context,
        });
        if let Some(ref bid) = batch_id {
            event_data["batch_id"] = serde_json::Value::String(bid.clone());
        }

        self.log_event_with_ts("life", name, &event_data, None)?;

        // Check batch completion and send launcher notification
        if launcher != "unknown" {
            if let Some(ref bid) = batch_id {
                self.check_batch_completion(&launcher, bid)?;
            }
        }

        Ok(())
    }

    /// Check if all instances in a launch batch are ready; send notification if so.
    pub fn check_batch_completion(&self, launcher: &str, batch_id: &str) -> Result<()> {
        // Find the launch event for this batch
        let launch_data: Option<String> = self
            .conn
            .query_row(
                "SELECT data FROM events
             WHERE type = 'life' AND instance = ?
               AND json_extract(data, '$.action') = 'batch_launched'
               AND json_extract(data, '$.batch_id') = ?
             LIMIT 1",
                params![launcher, batch_id],
                |row| row.get(0),
            )
            .ok();

        let Some(data_str) = launch_data else {
            return Ok(());
        };
        let data: serde_json::Value = serde_json::from_str(&data_str)?;
        let expected = data.get("launched").and_then(|v| v.as_u64()).unwrap_or(0);
        if expected == 0 {
            return Ok(());
        }

        // Count ready events with matching batch_id
        let ready_count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM events
             WHERE type = 'life'
               AND json_extract(data, '$.action') = 'ready'
               AND json_extract(data, '$.batch_id') = ?",
            params![batch_id],
            |row| row.get(0),
        )?;

        if (ready_count as u64) < expected {
            return Ok(());
        }

        // Check idempotency — don't send duplicate notification
        let already_sent: bool = self.conn.query_row(
            "SELECT COUNT(*) FROM events
             WHERE type = 'message'
               AND instance = 'sys_[hcom-launcher]'
               AND json_extract(data, '$.text') LIKE ?
             LIMIT 1",
            params![format!("%batch: {}%", batch_id)],
            |row| Ok(row.get::<_, i64>(0)? > 0),
        )?;

        if already_sent {
            return Ok(());
        }

        // Get instance names from this batch
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT instance FROM events
             WHERE type = 'life'
               AND json_extract(data, '$.action') = 'ready'
               AND json_extract(data, '$.batch_id') = ?",
        )?;
        let names: Vec<String> = stmt
            .query_map(params![batch_id], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        let instances_list = names.join(", ");
        let text = format!(
            "@{} All {} instances ready: {} (batch: {})",
            launcher, expected, instances_list, batch_id
        );

        // Insert system message
        let msg_data = serde_json::json!({
            "from": "[hcom-launcher]",
            "text": text,
            "scope": "mentions",
            "mentions": [launcher],
            "sender_kind": "system",
        });
        self.log_event_with_ts("message", "sys_[hcom-launcher]", &msg_data, None)?;

        Ok(())
    }

    /// Send a launcher-targeted notification for a failed launch instance.
    ///
    /// Used for early PTY startup failures so the launcher gets an active signal
    /// instead of having to poll `events launch`.
    pub fn notify_batch_failure(
        &self,
        launcher: &str,
        batch_id: &str,
        instance_name: &str,
        detail: &str,
    ) -> Result<()> {
        let text = format!(
            "@{} Launch failed: {}: {} (batch: {})",
            launcher, instance_name, detail, batch_id
        );

        let already_sent: bool = self.conn.query_row(
            "SELECT COUNT(*) FROM events
             WHERE type = 'message'
               AND instance = 'sys_[hcom-launcher]'
               AND json_extract(data, '$.text') = ?
             LIMIT 1",
            params![text],
            |row| Ok(row.get::<_, i64>(0)? > 0),
        )?;

        if already_sent {
            return Ok(());
        }

        let msg_data = serde_json::json!({
            "from": "[hcom-launcher]",
            "text": text,
            "scope": "mentions",
            "mentions": [launcher],
            "sender_kind": "system",
        });
        self.log_event_with_ts("message", "sys_[hcom-launcher]", &msg_data, None)?;

        Ok(())
    }

    /// Update gate blocking status WITHOUT logging a status event.
    ///
    /// Used for transient PTY gate states (tui:*) that shouldn't pollute the events table.
    /// Only updates the instance row; TUI reads this for display but no event is created.
    ///
    /// Args:
    ///   context: Gate context like "tui:not-ready", "tui:user-active", etc.
    ///   detail: Human-readable description like "user is typing"
    /// Preserve status_detail when it's "cmd:listen" — gate diagnostics must not
    /// overwrite the flag that blocks PTY injection during `hcom listen`.
    pub fn set_gate_status(&self, name: &str, context: &str, detail: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE instances SET status_context = ?,
                status_detail = CASE WHEN status_detail = 'cmd:listen' THEN status_detail ELSE ? END
             WHERE name = ?",
            params![context, detail, name],
        )?;
        Ok(())
    }

    /// Update instance PID after spawn
    pub fn update_instance_pid(&self, name: &str, pid: u32) -> Result<()> {
        self.conn.execute(
            "UPDATE instances SET pid = ? WHERE name = ?",
            params![pid as i64, name],
        )?;
        Ok(())
    }

    /// Store launch_context JSON (terminal preset, pane_id, env snapshot).
    /// Merges incoming keys into existing JSON, only filling fields that are
    /// currently missing or empty so late-bound PTY metadata can be persisted
    /// without clobbering richer hook-captured context.
    pub fn store_launch_context(&self, name: &str, context_json: &str) -> Result<()> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            let existing_json: Option<String> = self
                .conn
                .query_row(
                    "SELECT launch_context FROM instances WHERE name = ?",
                    params![name],
                    |row| row.get(0),
                )
                .optional()?;

            let existing_json = existing_json.unwrap_or_default();
            if existing_json.is_empty() {
                self.conn.execute(
                    "UPDATE instances SET launch_context = ? WHERE name = ?",
                    params![context_json, name],
                )?;
                return Ok(());
            }

            let mut existing = match serde_json::from_str::<
                serde_json::Map<String, serde_json::Value>,
            >(&existing_json)
            {
                Ok(map) => map,
                Err(_) => return Ok(()),
            };
            let incoming = match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
                context_json,
            ) {
                Ok(map) => map,
                Err(_) => return Ok(()),
            };

            let mut changed = false;
            for (key, value) in incoming {
                let should_fill = existing.get(&key).is_none_or(|current| {
                    current.is_null() || current.as_str().is_some_and(str::is_empty)
                });
                if should_fill {
                    existing.insert(key, value);
                    changed = true;
                }
            }

            if changed {
                self.conn.execute(
                    "UPDATE instances SET launch_context = ? WHERE name = ?",
                    params![
                        serde_json::to_string(&existing).unwrap_or_else(|_| "{}".to_string()),
                        name
                    ],
                )?;
            }
            Ok(())
        })();

        match result {
            Ok(()) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(())
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Get instance tag (for display name computation).
    /// Returns Some(tag) if tag is non-empty, None otherwise.
    pub fn get_instance_tag(&self, name: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT tag FROM instances WHERE name = ?",
                params![name],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
            .filter(|t| !t.is_empty())
    }

    /// Get current status and context for gate blocking logic
    ///
    /// Returns:
    /// - Ok(Some((status, context))) if instance exists
    /// - Ok(None) if instance not found
    /// - Err if database error occurs
    pub fn get_status(&self, name: &str) -> Result<Option<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT status, status_context FROM instances WHERE name = ?")?;

        match stmt.query_row(params![name], |row| {
            Ok((
                row.get::<_, String>(0)
                    .unwrap_or_else(|_| "unknown".to_string()),
                row.get::<_, String>(1).unwrap_or_default(),
            ))
        }) {
            Ok(status) => Ok(Some(status)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Delete process binding (for cleanup)
    pub fn delete_process_binding(&self, process_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM process_bindings WHERE process_id = ?",
            params![process_id],
        )?;
        Ok(())
    }

    /// Get process binding to check for name changes
    ///
    /// Returns:
    /// - Ok(Some(instance_name)) if binding exists
    /// - Ok(None) if binding not found
    /// - Err if database error occurs
    pub fn get_process_binding(&self, process_id: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT instance_name FROM process_bindings WHERE process_id = ?")?;

        match stmt.query_row(params![process_id], |row| row.get::<_, String>(0)) {
            Ok(name) => Ok(Some(name)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get process binding with session_id. Returns (session_id, instance_name).
    pub fn get_process_binding_full(
        &self,
        process_id: &str,
    ) -> Result<Option<(Option<String>, String)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT session_id, instance_name FROM process_bindings WHERE process_id = ?",
        )?;

        match stmt.query_row(params![process_id], |row| {
            Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?))
        }) {
            Ok(pair) => Ok(Some(pair)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Migrate notify endpoints from old instance to new instance
    pub fn migrate_notify_endpoints(&self, old_name: &str, new_name: &str) -> Result<()> {
        if old_name == new_name {
            return Ok(());
        }

        // Delete existing endpoints for new name
        self.conn.execute(
            "DELETE FROM notify_endpoints WHERE instance = ?",
            params![new_name],
        )?;

        // Move endpoints from old to new
        self.conn.execute(
            "UPDATE notify_endpoints SET instance = ? WHERE instance = ?",
            params![new_name, old_name],
        )?;

        Ok(())
    }

    /// Get last_event_id for an instance (cursor position for message delivery).
    ///
    /// Returns 0 if instance not found or on error.
    pub fn get_cursor(&self, name: &str) -> i64 {
        match self.get_instance_status(name) {
            Ok(Some(status)) => status.last_event_id,
            Ok(None) => 0, // No instance found
            Err(e) => {
                crate::log::log_error("db", "get_cursor.get_instance_status", &format!("{e}"));
                0
            }
        }
    }

    /// Check if instance has a session binding (session_id is set and non-empty).
    /// Used by OpenCode delivery thread to skip PTY injection when plugin is active.
    pub fn has_session(&self, name: &str) -> bool {
        match self.conn.query_row(
            "SELECT session_id FROM instances WHERE name = ?",
            params![name],
            |row| row.get::<_, String>(0),
        ) {
            Ok(sid) => !sid.is_empty(),
            _ => false,
        }
    }

    /// Check if there are pending (unread) messages for an instance.
    ///
    /// Lightweight check — parses only the JSON `data` column (skipping full
    /// Message construction) and returns on the first matching row.
    pub fn has_pending(&self, name: &str) -> bool {
        let last_event_id = match self.get_instance_status(name) {
            Ok(Some(status)) => status.last_event_id,
            Ok(None) => 0,
            Err(e) => {
                crate::log::log_error("db", "has_pending.get_instance_status", &format!("{e}"));
                0
            }
        };

        let mut stmt = match self
            .conn
            .prepare_cached("SELECT data FROM events WHERE id > ? AND type = 'message'")
        {
            Ok(s) => s,
            Err(e) => {
                crate::log::log_error("db", "has_pending.prepare", &format!("{e}"));
                return false;
            }
        };

        let rows = match stmt.query_map(params![last_event_id], |row| row.get::<_, String>(0)) {
            Ok(r) => r,
            Err(e) => {
                crate::log::log_error("db", "has_pending.query", &format!("{e}"));
                return false;
            }
        };

        for data in rows.flatten() {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&data) {
                if Self::should_deliver_to(&json, name) {
                    return true;
                }
            }
        }
        false
    }

    /// Get transcript path for an instance
    ///
    /// Returns:
    /// - Ok(Some(path)) if instance exists and has non-empty transcript_path
    /// - Ok(None) if instance not found or transcript_path is empty
    /// - Err if database error occurs
    pub fn get_transcript_path(&self, name: &str) -> Result<Option<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT transcript_path FROM instances WHERE name = ?")?;

        match stmt.query_row(params![name], |row| row.get::<_, String>(0)) {
            Ok(path) if !path.is_empty() => Ok(Some(path)),
            Ok(_) => Ok(None), // Empty path
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get instance snapshot for life event logging before deletion
    ///
    /// Returns:
    /// - Ok(Some(snapshot)) if instance exists
    /// - Ok(None) if instance not found
    /// - Err if database error occurs
    pub fn get_instance_snapshot(&self, name: &str) -> Result<Option<serde_json::Value>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT transcript_path, session_id, tool, directory, parent_name, tag,
                    wait_timeout, subagent_timeout, hints, pid, created_at, background,
                    agent_id, launch_args, origin_device_id, background_log_file, last_event_id
             FROM instances WHERE name = ?",
        )?;

        match stmt.query_row(params![name], |row| {
            Ok(serde_json::json!({
                "transcript_path": row.get::<_, String>(0).unwrap_or_default(),
                "session_id": row.get::<_, String>(1).unwrap_or_default(),
                "tool": row.get::<_, String>(2).unwrap_or_default(),
                "directory": row.get::<_, String>(3).unwrap_or_default(),
                "parent_name": row.get::<_, String>(4).unwrap_or_default(),
                "tag": row.get::<_, String>(5).unwrap_or_default(),
                "wait_timeout": row.get::<_, Option<i64>>(6).unwrap_or(None),
                "subagent_timeout": row.get::<_, Option<i64>>(7).unwrap_or(None),
                "hints": row.get::<_, String>(8).unwrap_or_default(),
                "pid": row.get::<_, Option<i64>>(9).unwrap_or(None),
                "created_at": row.get::<_, f64>(10).unwrap_or(0.0),
                "background": row.get::<_, i64>(11).unwrap_or(0),
                "agent_id": row.get::<_, String>(12).unwrap_or_default(),
                "launch_args": row.get::<_, String>(13).unwrap_or_default(),
                "origin_device_id": row.get::<_, String>(14).unwrap_or_default(),
                "background_log_file": row.get::<_, String>(15).unwrap_or_default(),
                "last_event_id": row.get::<_, i64>(16).unwrap_or(0),
            }))
        }) {
            Ok(snapshot) => Ok(Some(snapshot)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Delete instance row from database
    pub fn delete_instance(&self, name: &str) -> Result<bool> {
        let rows = self
            .conn
            .execute("DELETE FROM instances WHERE name = ?", params![name])?;
        Ok(rows > 0)
    }

    /// Log a life event (started/stopped) to the events table
    pub fn log_life_event(
        &self,
        instance: &str,
        action: &str,
        by: &str,
        reason: &str,
        snapshot: Option<serde_json::Value>,
    ) -> Result<()> {
        let data = match snapshot {
            Some(s) => serde_json::json!({
                "action": action,
                "by": by,
                "reason": reason,
                "snapshot": s
            }),
            None => serde_json::json!({
                "action": action,
                "by": by,
                "reason": reason
            }),
        };

        self.log_event_with_ts("life", instance, &data, None)?;

        Ok(())
    }

    /// Check whether `name`'s *current* identity is a subagent slot.
    ///
    /// Classification rules, in order:
    /// 1. If a live instance row exists, it defines the current identity:
    ///    non-empty `parent_name` → true (subagent); empty → false
    ///    (top-level, regardless of any historical subagent events).
    /// 2. Otherwise, consult the *most recent* `life.stopped` event: true
    ///    iff its snapshot has a non-empty `agent_id`.
    ///
    /// Older subagent history does not poison a name that has since been
    /// reused top-level (either live or via a more recent top-level stop).
    pub fn was_subagent_name(&self, name: &str) -> bool {
        if let Ok(Some(data)) = self.get_instance(name) {
            return data
                .get("parent_name")
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.is_empty());
        }

        self.conn
            .query_row(
                "SELECT COALESCE(json_extract(data, '$.snapshot.agent_id'), '') != ''
                 FROM events
                 WHERE type = 'life'
                   AND instance = ?
                   AND json_extract(data, '$.action') = 'stopped'
                 ORDER BY id DESC LIMIT 1",
                params![name],
                |row| row.get::<_, bool>(0),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or(false)
    }

    /// Find the most recent stopped instance whose snapshot carries the given
    /// session_id. life.stopped events are the source of truth: they persist
    /// across the `session_bindings` cascade, so they're the right thing to
    /// consult when reclaiming hcom identity by UUID after stop/kill.
    pub fn find_stopped_instance_by_session_id(&self, session_id: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT instance FROM events
                 WHERE type = 'life'
                   AND json_extract(data, '$.action') = 'stopped'
                   AND json_extract(data, '$.snapshot.session_id') = ?
                 ORDER BY id DESC LIMIT 1",
                params![session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(Into::into)
    }

    /// Delete notify endpoints for an instance
    pub fn delete_notify_endpoints(&self, name: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM notify_endpoints WHERE instance = ?",
            params![name],
        )?;
        Ok(())
    }

    /// Insert or update a notify endpoint with specific kind.
    /// Used by listen command to register listen/listen_filter endpoints.
    pub fn upsert_notify_endpoint(&self, name: &str, kind: &str, port: u16) -> Result<()> {
        let now = now_epoch_f64();

        self.conn.execute(
            "INSERT INTO notify_endpoints (instance, kind, port, updated_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(instance, kind) DO UPDATE SET
                 port = excluded.port,
                 updated_at = excluded.updated_at",
            params![name, kind, port as i64, now],
        )?;
        Ok(())
    }

    /// Delete a specific notify endpoint by instance and kind.
    pub fn delete_notify_endpoint(&self, name: &str, kind: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM notify_endpoints WHERE instance = ? AND kind = ?",
            params![name, kind],
        )?;
        Ok(())
    }

    /// Remove all event subscriptions owned by an instance.
    ///
    /// Subscriptions are stored as kv entries with key 'events_sub:sub-{hash}'
    /// and a JSON value containing a "caller" field.
    pub fn cleanup_subscriptions(&self, name: &str) -> Result<u32> {
        // Delegates to db::subscriptions; events_sub: kv ownership lives there.
        subscriptions::cleanup_subscriptions(self, name)
    }

    /// Remove delivery-only thread memberships for an instance.
    ///
    /// This is used when a stopped name is being reused by a fresh instance:
    /// normal stop/resume should preserve memberships, but identity replacement
    /// must not inherit old thread state.
    pub fn cleanup_thread_memberships_for_name_reuse(&self, name: &str) -> Result<u32> {
        // Delegates to db::subscriptions; events_sub: kv ownership lives there.
        subscriptions::cleanup_thread_memberships_for_name_reuse(self, name)
    }

    /// Return active members of a thread in join order.
    pub fn get_thread_members(&self, thread: &str) -> Vec<String> {
        // Delegates to db::subscriptions; events_sub: kv ownership lives there.
        subscriptions::get_thread_members(self, thread)
    }

    /// Upsert memberships for recipients of a thread message.
    pub fn add_thread_memberships(
        &self,
        thread: &str,
        sender: Option<&str>,
        recipients: &[String],
    ) {
        // Delegates to db::subscriptions; events_sub: kv ownership lives there.
        subscriptions::add_thread_memberships(self, thread, sender, recipients);
    }

    /// Get value from kv table.
    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        match self
            .conn
            .query_row("SELECT value FROM kv WHERE key = ?", params![key], |row| {
                row.get::<_, Option<String>>(0)
            }) {
            Ok(val) => Ok(val),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Set or delete value in kv table. Pass None to delete.
    pub fn kv_set(&self, key: &str, value: Option<&str>) -> Result<()> {
        match value {
            None => {
                self.conn
                    .execute("DELETE FROM kv WHERE key = ?", params![key])?;
            }
            Some(v) => {
                self.conn.execute(
                    "INSERT OR REPLACE INTO kv (key, value) VALUES (?, ?)",
                    params![key, v],
                )?;
            }
        }
        Ok(())
    }

    /// Get all kv entries whose key starts with prefix. Returns Vec<(key, value)>.
    pub fn kv_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        // Escape LIKE wildcards (%, _, \) in prefix to avoid unintended matches
        let escaped = prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("{}%", escaped);
        let mut stmt = self
            .conn
            .prepare_cached("SELECT key, value FROM kv WHERE key LIKE ? ESCAPE '\\'")?;
        let rows = stmt
            .query_map(params![pattern], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Get instance name bound to session_id, or None if not bound.
    pub fn get_session_binding(&self, session_id: &str) -> Result<Option<String>> {
        if session_id.is_empty() {
            return Ok(None);
        }
        match self.conn.query_row(
            "SELECT instance_name FROM session_bindings WHERE session_id = ?",
            params![session_id],
            |row| row.get::<_, String>(0),
        ) {
            Ok(name) => Ok(Some(name)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Create or update session binding.
    /// Returns error if session_id is already bound to a different instance.
    pub fn set_session_binding(&self, session_id: &str, instance_name: &str) -> Result<()> {
        if session_id.is_empty() || instance_name.is_empty() {
            return Ok(());
        }

        // Check for existing binding to different instance
        if let Some(existing) = self.get_session_binding(session_id)? {
            if existing != instance_name {
                // Check if this is a subagent trying to bind without --name <agent_id>
                if let Ok(Some(inst)) = self.get_instance(&existing) {
                    if let Some(rt) = inst.get("running_tasks").and_then(|v| v.as_str()) {
                        if let Ok(tasks) = serde_json::from_str::<serde_json::Value>(rt) {
                            if let Some(subs) = tasks.get("subagents").and_then(|v| v.as_array()) {
                                if !subs.is_empty() {
                                    let ids: Vec<&str> = subs
                                        .iter()
                                        .filter_map(|s| s.get("agent_id").and_then(|v| v.as_str()))
                                        .collect();
                                    bail!(
                                        "Session bound to parent '{}'. \
                                         Subagents must use: hcom start --name <agent_id>\n\
                                         Active agent_ids: {}",
                                        existing,
                                        ids.join(", ")
                                    );
                                }
                            }
                        }
                    }
                }
                bail!(
                    "Session {}... already bound to {}, cannot bind to {}",
                    &session_id[..session_id.len().min(8)],
                    existing,
                    instance_name
                );
            }
        }

        let now = now_epoch_f64();

        self.conn.execute(
            "INSERT INTO session_bindings (session_id, instance_name, created_at)
             VALUES (?, ?, ?)
             ON CONFLICT(session_id) DO UPDATE SET
                 instance_name = excluded.instance_name,
                 created_at = excluded.created_at",
            params![session_id, instance_name, now],
        )?;
        Ok(())
    }

    /// Clear session_id from any instance except exclude_instance.
    pub fn clear_session_id_from_other_instances(
        &self,
        session_id: &str,
        exclude_instance: &str,
    ) -> Result<()> {
        if session_id.is_empty() {
            return Ok(());
        }
        self.conn.execute(
            "UPDATE instances SET session_id = NULL WHERE session_id = ? AND name != ?",
            params![session_id, exclude_instance],
        )?;
        Ok(())
    }

    /// Explicitly rebind session to a different instance.
    pub fn rebind_session(&self, session_id: &str, new_instance_name: &str) -> Result<()> {
        if session_id.is_empty() || new_instance_name.is_empty() {
            return Ok(());
        }
        self.clear_session_id_from_other_instances(session_id, new_instance_name)?;
        self.upsert_session_binding(session_id, new_instance_name)
    }

    /// Internal helper: unconditional upsert of session binding.
    fn upsert_session_binding(&self, session_id: &str, instance_name: &str) -> Result<()> {
        let now = now_epoch_f64();
        self.conn.execute(
            "INSERT INTO session_bindings (session_id, instance_name, created_at)
             VALUES (?, ?, ?)
             ON CONFLICT(session_id) DO UPDATE SET
                 instance_name = excluded.instance_name,
                 created_at = excluded.created_at",
            params![session_id, instance_name, now],
        )?;
        Ok(())
    }

    /// Delete session binding.
    pub fn delete_session_binding(&self, session_id: &str) -> Result<()> {
        if session_id.is_empty() {
            return Ok(());
        }
        self.conn.execute(
            "DELETE FROM session_bindings WHERE session_id = ?",
            params![session_id],
        )?;
        Ok(())
    }

    /// Delete all session bindings for an instance.
    pub fn delete_session_bindings_for_instance(&self, instance_name: &str) -> Result<()> {
        if instance_name.is_empty() {
            return Ok(());
        }
        self.conn.execute(
            "DELETE FROM session_bindings WHERE instance_name = ?",
            params![instance_name],
        )?;
        Ok(())
    }

    /// Atomically rebind instance to new session.
    pub fn rebind_instance_session(&self, instance_name: &str, session_id: &str) -> Result<()> {
        if instance_name.is_empty() || session_id.is_empty() {
            return Ok(());
        }
        self.conn.execute(
            "DELETE FROM session_bindings WHERE instance_name = ?",
            params![instance_name],
        )?;
        self.conn.execute(
            "UPDATE instances SET session_id = NULL WHERE session_id = ? AND name != ?",
            params![session_id, instance_name],
        )?;
        self.upsert_session_binding(session_id, instance_name)?;
        Ok(())
    }

    /// Check if instance has a session binding (hooks active).
    pub fn has_session_binding(&self, instance_name: &str) -> bool {
        if instance_name.is_empty() {
            return false;
        }
        self.conn
            .query_row(
                "SELECT 1 FROM session_bindings WHERE instance_name = ? LIMIT 1",
                params![instance_name],
                |_| Ok(()),
            )
            .is_ok()
    }

    /// Check if instance has a process binding (hcom-launched).
    pub fn has_process_binding_for_instance(&self, instance_name: &str) -> bool {
        if instance_name.is_empty() {
            return false;
        }
        self.conn
            .query_row(
                "SELECT 1 FROM process_bindings WHERE instance_name = ? LIMIT 1",
                params![instance_name],
                |_| Ok(()),
            )
            .is_ok()
    }

    /// Set process binding (map process_id -> instance/session).
    /// Set process binding. Empty session_id is stored as NULL.
    pub fn set_process_binding(
        &self,
        process_id: &str,
        session_id: &str,
        instance_name: &str,
    ) -> Result<()> {
        let now = now_epoch_f64();
        // Normalize empty string to NULL
        let sid: Option<&str> = if session_id.is_empty() {
            None
        } else {
            Some(session_id)
        };
        self.conn.execute(
            "INSERT OR REPLACE INTO process_bindings (process_id, session_id, instance_name, updated_at)
             VALUES (?, ?, ?, ?)",
            params![process_id, sid, instance_name, now],
        )?;
        Ok(())
    }

    /// Delete all process bindings for an instance.
    pub fn delete_process_bindings_for_instance(&self, instance_name: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM process_bindings WHERE instance_name = ?",
            params![instance_name],
        )?;
        Ok(())
    }

    /// Insert event and return its ID. Calls subscription check inline.
    pub fn log_event(
        &self,
        event_type: &str,
        instance: &str,
        data: &serde_json::Value,
    ) -> Result<i64> {
        self.log_event_with_ts(event_type, instance, data, None)
    }

    /// Insert event with optional timestamp. Returns event ID.
    pub fn log_event_with_ts(
        &self,
        event_type: &str,
        instance: &str,
        data: &serde_json::Value,
        timestamp: Option<&str>,
    ) -> Result<i64> {
        let ts = match timestamp {
            Some(t) => t.to_string(),
            None => chrono_now_iso(),
        };
        let data_str = serde_json::to_string(data)?;

        self.conn.execute(
            "INSERT INTO events (timestamp, type, instance, data) VALUES (?, ?, ?, ?)",
            params![ts, event_type, instance, data_str],
        )?;
        let event_id = self.conn.last_insert_rowid();

        // Check event subscriptions inline.
        subscriptions::process_logged_event(self, event_id, event_type, instance, data);

        Ok(event_id)
    }

    /// Get events since a given ID with optional filters.
    pub fn get_events_since(
        &self,
        last_event_id: i64,
        event_type: Option<&str>,
        instance: Option<&str>,
    ) -> Result<Vec<serde_json::Value>> {
        let mut query =
            "SELECT id, timestamp, type, instance, data FROM events WHERE id > ?".to_string();
        let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(last_event_id)];

        if let Some(et) = event_type {
            query.push_str(" AND type = ?");
            param_values.push(Box::new(et.to_string()));
        }
        if let Some(inst) = instance {
            query.push_str(" AND instance = ?");
            param_values.push(Box::new(inst.to_string()));
        }
        query.push_str(" ORDER BY id");

        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            param_values.iter().map(|p| p.as_ref()).collect();
        let mut stmt = self.conn.prepare(&query)?;
        let rows = stmt
            .query_map(params_refs.as_slice(), |row| {
                let id: i64 = row.get(0)?;
                let timestamp: String = row.get(1)?;
                let etype: String = row.get(2)?;
                let inst: String = row.get(3)?;
                let data_str: String = row.get(4)?;
                Ok((id, timestamp, etype, inst, data_str))
            })?
            .filter_map(|r| r.ok())
            .map(|(id, timestamp, etype, inst, data_str)| {
                let data: serde_json::Value =
                    serde_json::from_str(&data_str).unwrap_or(serde_json::Value::Null);
                serde_json::json!({
                    "id": id,
                    "timestamp": timestamp,
                    "type": etype,
                    "instance": inst,
                    "data": data,
                })
            })
            .collect();
        Ok(rows)
    }

    /// Get current maximum event ID, or 0 if no events.
    pub fn get_last_event_id(&self) -> i64 {
        self.conn
            .query_row("SELECT MAX(id) FROM events", [], |row| {
                row.get::<_, Option<i64>>(0)
            })
            .unwrap_or(None)
            .unwrap_or(0)
    }

    /// Send a system notification message (simplified inline version).
    /// Parses @mentions, computes scope, inserts message event.
    pub fn send_system_message(&self, sender_name: &str, message: &str) -> Result<Vec<String>> {
        // Delegates to db::subscriptions; events_sub: kv ownership lives there.
        subscriptions::send_system_message(self, sender_name, message)
    }

    /// Like `send_system_message` but lets the caller specify `sender_kind`
    /// ("instance" | "external" | "system"). Used by subscription on-hit to
    /// preserve the sub caller's real identity on the event.
    pub fn send_message_as(
        &self,
        sender_name: &str,
        sender_kind: &str,
        message: &str,
    ) -> Result<Vec<String>> {
        // Delegates to db::subscriptions; events_sub: kv ownership lives there.
        subscriptions::send_message_as(self, sender_name, sender_kind, message)
    }

    /// Get instance by name. Returns full row as JSON or None.
    /// Column list for instance SELECT queries. Must match instance_row_to_json index order.
    const INSTANCE_COLUMNS: &str =
        "name, session_id, parent_session_id, parent_name, tag, last_event_id,
         status, status_time, status_context, status_detail, last_stop, directory,
         created_at, transcript_path, tcp_mode, wait_timeout, background,
         background_log_file, name_announced, agent_id, running_tasks,
         origin_device_id, hints, subagent_timeout, tool, launch_args,
         terminal_preset_requested, terminal_preset_effective,
         idle_since, pid, launch_context";

    /// Convert a row from INSTANCE_COLUMNS SELECT to JSON.
    fn instance_row_to_json(row: &rusqlite::Row) -> rusqlite::Result<serde_json::Value> {
        Ok(serde_json::json!({
            "name": row.get::<_, String>(0).unwrap_or_default(),
            "session_id": row.get::<_, Option<String>>(1).unwrap_or(None),
            "parent_session_id": row.get::<_, Option<String>>(2).unwrap_or(None),
            "parent_name": row.get::<_, Option<String>>(3).unwrap_or(None),
            "tag": row.get::<_, Option<String>>(4).unwrap_or(None),
            "last_event_id": row.get::<_, i64>(5).unwrap_or(0),
            "status": row.get::<_, String>(6).unwrap_or_default(),
            "status_time": row.get::<_, i64>(7).unwrap_or(0),
            "status_context": row.get::<_, String>(8).unwrap_or_default(),
            "status_detail": row.get::<_, String>(9).unwrap_or_default(),
            "last_stop": row.get::<_, i64>(10).unwrap_or(0),
            "directory": row.get::<_, Option<String>>(11).unwrap_or(None),
            "created_at": row.get::<_, f64>(12).unwrap_or(0.0),
            "transcript_path": row.get::<_, String>(13).unwrap_or_default(),
            "tcp_mode": row.get::<_, i64>(14).unwrap_or(0),
            "wait_timeout": row.get::<_, i64>(15).unwrap_or(86400),
            "background": row.get::<_, i64>(16).unwrap_or(0),
            "background_log_file": row.get::<_, String>(17).unwrap_or_default(),
            "name_announced": row.get::<_, i64>(18).unwrap_or(0),
            "agent_id": row.get::<_, Option<String>>(19).unwrap_or(None),
            "running_tasks": row.get::<_, String>(20).unwrap_or_default(),
            "origin_device_id": row.get::<_, String>(21).unwrap_or_default(),
            "hints": row.get::<_, String>(22).unwrap_or_default(),
            "subagent_timeout": row.get::<_, Option<i64>>(23).unwrap_or(None),
            "tool": row.get::<_, String>(24).unwrap_or_default(),
            "launch_args": row.get::<_, String>(25).unwrap_or_default(),
            "terminal_preset_requested": row.get::<_, String>(26).unwrap_or_default(),
            "terminal_preset_effective": row.get::<_, String>(27).unwrap_or_default(),
            "idle_since": row.get::<_, String>(28).unwrap_or_default(),
            "pid": row.get::<_, Option<i64>>(29).unwrap_or(None),
            "launch_context": row.get::<_, String>(30).unwrap_or_default(),
        }))
    }

    pub fn get_instance(&self, name: &str) -> Result<Option<serde_json::Value>> {
        let sql = format!(
            "SELECT {} FROM instances WHERE name = ?",
            Self::INSTANCE_COLUMNS
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;

        match stmt.query_row(params![name], Self::instance_row_to_json) {
            Ok(inst) => Ok(Some(inst)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Look up instance name by agent_id (Claude Code sends short IDs like 'a6d9caf').
    pub fn get_instance_by_agent_id(&self, agent_id: &str) -> Result<Option<String>> {
        match self.conn.query_row(
            "SELECT name FROM instances WHERE agent_id = ?",
            params![agent_id],
            |row| row.get::<_, String>(0),
        ) {
            Ok(name) => Ok(Some(name)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Save (upsert) instance from a map of field names to values.
    pub fn save_instance(
        &self,
        data: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<bool> {
        if data.is_empty() {
            return Ok(false);
        }

        let columns: Vec<&str> = data.keys().map(|k| k.as_str()).collect();
        for col in &columns {
            Self::validate_column(col)?;
        }
        let placeholders = vec!["?"; columns.len()].join(", ");
        let update_clause: String = columns
            .iter()
            .filter(|&&k| k != "name")
            .map(|k| format!("{} = excluded.{}", k, k))
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "INSERT INTO instances ({}) VALUES ({}) ON CONFLICT(name) DO UPDATE SET {}",
            columns.join(", "),
            placeholders,
            update_clause
        );

        let values: Vec<Box<dyn rusqlite::types::ToSql>> = columns
            .iter()
            .map(|&col| -> Box<dyn rusqlite::types::ToSql> {
                let val = &data[col];
                match val {
                    serde_json::Value::String(s) => Box::new(s.clone()),
                    serde_json::Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            Box::new(i)
                        } else if let Some(f) = n.as_f64() {
                            Box::new(f)
                        } else {
                            Box::new(val.to_string())
                        }
                    }
                    serde_json::Value::Bool(b) => Box::new(*b as i32),
                    serde_json::Value::Null => Box::new(rusqlite::types::Null),
                    _ => Box::new(val.to_string()),
                }
            })
            .collect();

        let refs: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|v| v.as_ref()).collect();
        self.conn.execute(&sql, refs.as_slice())?;
        Ok(true)
    }

    /// Update specific instance fields.
    pub fn update_instance(
        &self,
        name: &str,
        updates: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<bool> {
        if updates.is_empty() {
            return Ok(true);
        }

        let entries: Vec<(&String, &serde_json::Value)> = updates.iter().collect();
        for (k, _) in &entries {
            Self::validate_column(k)?;
        }

        let set_clause: String = entries
            .iter()
            .map(|(k, _)| format!("{} = ?", k))
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!("UPDATE instances SET {} WHERE name = ?", set_clause);

        let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = entries
            .iter()
            .map(|(_, val)| -> Box<dyn rusqlite::types::ToSql> {
                match val {
                    serde_json::Value::String(s) => Box::new(s.clone()),
                    serde_json::Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            Box::new(i)
                        } else if let Some(f) = n.as_f64() {
                            Box::new(f)
                        } else {
                            Box::new(val.to_string())
                        }
                    }
                    serde_json::Value::Bool(b) => Box::new(*b as i32),
                    serde_json::Value::Null => Box::new(rusqlite::types::Null),
                    _ => Box::new(val.to_string()),
                }
            })
            .collect();
        values.push(Box::new(name.to_string()));

        let refs: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|v| v.as_ref()).collect();
        self.conn.execute(&sql, refs.as_slice())?;
        Ok(true)
    }

    /// Iterate all instances, returning Vec of JSON objects.
    pub fn iter_instances(&self) -> Result<Vec<serde_json::Value>> {
        let sql = format!(
            "SELECT {} FROM instances ORDER BY created_at DESC",
            Self::INSTANCE_COLUMNS
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;

        let rows: Vec<serde_json::Value> = stmt
            .query_map([], Self::instance_row_to_json)?
            .filter_map(|r| r.ok())
            .collect();

        Ok(rows)
    }

    /// Log a status event to the events table
    ///
    /// Used by TranscriptWatcher to log tool:apply_patch, tool:shell, and prompt events.
    pub fn log_status_event(
        &self,
        instance: &str,
        status: &str,
        context: &str,
        detail: Option<&str>,
        timestamp: Option<&str>,
    ) -> Result<()> {
        // Build data JSON
        let data = match detail {
            Some(d) => serde_json::json!({
                "status": status,
                "context": context,
                "detail": d
            }),
            None => serde_json::json!({
                "status": status,
                "context": context
            }),
        };

        self.log_event_with_ts("status", instance, &data, timestamp)?;

        Ok(())
    }

    /// Get full instance row by name.
    pub fn get_instance_full(&self, name: &str) -> Result<Option<InstanceRow>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT * FROM instances WHERE name = ?")?;
        match stmt.query_row(params![name], InstanceRow::from_row) {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get all instance rows.
    pub fn iter_instances_full(&self) -> Result<Vec<InstanceRow>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT * FROM instances ORDER BY created_at DESC")?;
        let rows = stmt
            .query_map([], InstanceRow::from_row)?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    }

    /// Save (INSERT OR REPLACE) an instance row.
    /// Uses a JSON Value map for flexible field specification.
    pub fn save_instance_named(
        &self,
        name: &str,
        data: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<bool> {
        // Build column list and values dynamically
        let mut cols = vec!["name"];
        let mut placeholders = vec!["?"];
        let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(name.to_string())];

        for (key, val) in data {
            if key == "name" {
                continue;
            }
            cols.push(Self::validate_column(key)?);
            placeholders.push("?");
            values.push(Self::json_value_to_sql(val));
        }

        let sql = format!(
            "INSERT OR REPLACE INTO instances ({}) VALUES ({})",
            cols.join(", "),
            placeholders.join(", ")
        );

        let refs: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|b| b.as_ref()).collect();
        let rows = self.conn.execute(&sql, refs.as_slice())?;
        Ok(rows > 0)
    }

    /// Update specific fields on an instance row.
    /// Uses a JSON Value map for flexible field specification.
    pub fn update_instance_fields(
        &self,
        name: &str,
        updates: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }

        let mut set_parts = Vec::new();
        let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        for (key, val) in updates {
            set_parts.push(format!("{} = ?", Self::validate_column(key)?));
            values.push(Self::json_value_to_sql(val));
        }

        values.push(Box::new(name.to_string()));

        let sql = format!(
            "UPDATE instances SET {} WHERE name = ?",
            set_parts.join(", ")
        );

        let refs: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|b| b.as_ref()).collect();
        self.conn.execute(&sql, refs.as_slice())?;
        Ok(())
    }

    /// Validate column name against SQL injection (whitelist of known columns).
    fn validate_column(key: &str) -> Result<&str> {
        const VALID_COLUMNS: &[&str] = &[
            "name",
            "session_id",
            "parent_session_id",
            "parent_name",
            "agent_id",
            "tag",
            "last_event_id",
            "last_stop",
            "status",
            "status_time",
            "status_context",
            "status_detail",
            "directory",
            "created_at",
            "transcript_path",
            "tool",
            "background",
            "background_log_file",
            "tcp_mode",
            "wait_timeout",
            "subagent_timeout",
            "hints",
            "origin_device_id",
            "pid",
            "launch_args",
            "launch_context",
            "name_announced",
            "running_tasks",
            "idle_since",
            "terminal_preset_requested",
            "terminal_preset_effective",
        ];
        if VALID_COLUMNS.contains(&key) {
            Ok(key)
        } else {
            Err(anyhow::anyhow!("Invalid column name: {}", key))
        }
    }

    /// Convert a serde_json::Value to a boxed ToSql for dynamic SQL binding.
    fn json_value_to_sql(val: &serde_json::Value) -> Box<dyn rusqlite::types::ToSql> {
        match val {
            serde_json::Value::Null => Box::new(rusqlite::types::Null),
            serde_json::Value::Bool(b) => Box::new(if *b { 1i64 } else { 0i64 }),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Box::new(i)
                } else if let Some(f) = n.as_f64() {
                    Box::new(f)
                } else {
                    Box::new(rusqlite::types::Null)
                }
            }
            serde_json::Value::String(s) => Box::new(s.clone()),
            _ => Box::new(val.to_string()),
        }
    }

    /// Check if any notify endpoint exists for an instance.
    pub fn has_notify_endpoint(&self, name: &str) -> bool {
        self.conn
            .query_row(
                "SELECT 1 FROM notify_endpoints WHERE instance = ? LIMIT 1",
                params![name],
                |_| Ok(()),
            )
            .is_ok()
    }
}

/// Generate ISO timestamp for current time.
fn chrono_now_iso() -> String {
    crate::shared::time::now_iso()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::path::PathBuf;

    /// Create a test database with instances table
    fn setup_test_db() -> (Connection, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let temp_dir = std::env::temp_dir();
        let test_id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let db_path = temp_dir.join(format!("test_hcom_{}_{}.db", std::process::id(), test_id));

        let conn = Connection::open(&db_path).unwrap();

        // Create minimal schema
        conn.execute_batch(
            "CREATE TABLE instances (
                name TEXT PRIMARY KEY,
                status TEXT,
                status_context TEXT,
                status_detail TEXT,
                last_event_id INTEGER,
                transcript_path TEXT,
                session_id TEXT,
                tool TEXT,
                directory TEXT,
                parent_name TEXT,
                tag TEXT,
                wait_timeout INTEGER,
                subagent_timeout INTEGER,
                hints TEXT,
                pid INTEGER,
                created_at TEXT,
                background INTEGER,
                agent_id TEXT,
                launch_args TEXT,
                terminal_preset_requested TEXT,
                terminal_preset_effective TEXT,
                launch_context TEXT,
                origin_device_id TEXT,
                background_log_file TEXT,
                status_time INTEGER
            );

            CREATE TABLE process_bindings (
                process_id TEXT PRIMARY KEY,
                session_id TEXT,
                instance_name TEXT,
                updated_at REAL NOT NULL
            );",
        )
        .unwrap();

        (conn, db_path)
    }

    /// Clean up test database
    fn cleanup_test_db(path: PathBuf) {
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_get_instance_status_propagates_prepare_error() {
        // Verify that SQL errors are propagated as Err (not silently converted to None)
        let (conn, db_path) = setup_test_db();

        // Drop the instances table to cause SQL error
        conn.execute("DROP TABLE instances", []).unwrap();
        drop(conn);

        // Now HcomDb will fail when trying to query
        let db = HcomDb::open_raw(&db_path).unwrap();

        let result = db.get_instance_status("test");

        // SQL error should be propagated as Err, not None
        let err = result.expect_err("SQL error should propagate as Err, not None");
        assert!(
            err.to_string().contains("instances"),
            "expected missing instances table error, got: {err:#}"
        );

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_get_instance_status_returns_ok_none_when_not_found() {
        // Verify that "not found" is distinguished from "error" via Ok(None)

        let (_conn, db_path) = setup_test_db();
        let db = HcomDb::open_raw(&db_path).unwrap();

        // Query non-existent instance
        let result = db.get_instance_status("nonexistent");

        // Should be Ok(None) - not found is not an error
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_get_status_propagates_prepare_error() {
        let (conn, db_path) = setup_test_db();
        conn.execute("DROP TABLE instances", []).unwrap();
        drop(conn);

        let db = HcomDb::open_raw(&db_path).unwrap();
        let result = db.get_status("test");

        let err = result.expect_err("SQL error should propagate as Err");
        assert!(
            err.to_string().contains("instances"),
            "expected missing instances table error, got: {err:#}"
        );
        cleanup_test_db(db_path);
    }

    #[test]
    fn test_get_process_binding_propagates_prepare_error() {
        let (conn, db_path) = setup_test_db();
        conn.execute("DROP TABLE process_bindings", []).unwrap();
        drop(conn);

        let db = HcomDb::open_raw(&db_path).unwrap();
        let result = db.get_process_binding("test_pid");

        let err = result.expect_err("SQL error should propagate as Err");
        assert!(
            err.to_string().contains("process_bindings"),
            "expected missing process_bindings table error, got: {err:#}"
        );
        cleanup_test_db(db_path);
    }

    #[test]
    fn test_get_transcript_path_propagates_prepare_error() {
        let (conn, db_path) = setup_test_db();
        conn.execute("DROP TABLE instances", []).unwrap();
        drop(conn);

        let db = HcomDb::open_raw(&db_path).unwrap();
        let result = db.get_transcript_path("test");

        let err = result.expect_err("SQL error should propagate as Err");
        assert!(
            err.to_string().contains("instances"),
            "expected missing instances table error, got: {err:#}"
        );
        cleanup_test_db(db_path);
    }

    #[test]
    fn test_get_instance_snapshot_propagates_prepare_error() {
        let (conn, db_path) = setup_test_db();
        conn.execute("DROP TABLE instances", []).unwrap();
        drop(conn);

        let db = HcomDb::open_raw(&db_path).unwrap();
        let result = db.get_instance_snapshot("test");

        let err = result.expect_err("SQL error should propagate as Err");
        assert!(
            err.to_string().contains("instances"),
            "expected missing instances table error, got: {err:#}"
        );
        cleanup_test_db(db_path);
    }

    #[test]
    fn test_all_methods_return_ok_none_when_not_found() {
        let (_conn, db_path) = setup_test_db();
        let db = HcomDb::open_raw(&db_path).unwrap();

        // All these should return Ok(None) for non-existent data
        assert!(db.get_instance_status("nonexistent").unwrap().is_none());
        assert!(db.get_status("nonexistent").unwrap().is_none());
        assert!(db.get_process_binding("nonexistent").unwrap().is_none());
        assert!(db.get_transcript_path("nonexistent").unwrap().is_none());
        assert!(db.get_instance_snapshot("nonexistent").unwrap().is_none());

        cleanup_test_db(db_path);
    }

    fn setup_test_db_with_endpoints() -> (Connection, PathBuf) {
        let (conn, db_path) = setup_test_db();
        conn.execute_batch(
            "CREATE TABLE notify_endpoints (
                instance TEXT NOT NULL,
                kind TEXT NOT NULL,
                port INTEGER NOT NULL,
                updated_at REAL NOT NULL,
                PRIMARY KEY (instance, kind)
            );",
        )
        .unwrap();
        (conn, db_path)
    }

    #[test]
    fn test_register_inject_port_inserts() {
        let (_conn, db_path) = setup_test_db_with_endpoints();
        let db = HcomDb::open_raw(&db_path).unwrap();

        db.register_inject_port("test", 5555).unwrap();

        let port: i64 = db
            .conn
            .query_row(
                "SELECT port FROM notify_endpoints WHERE instance = 'test' AND kind = 'inject'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(port, 5555);

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_register_inject_port_upserts() {
        let (_conn, db_path) = setup_test_db_with_endpoints();
        let db = HcomDb::open_raw(&db_path).unwrap();

        db.register_inject_port("test", 5555).unwrap();
        db.register_inject_port("test", 6666).unwrap();

        let port: i64 = db
            .conn
            .query_row(
                "SELECT port FROM notify_endpoints WHERE instance = 'test' AND kind = 'inject'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(port, 6666);

        // Should be exactly one row
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM notify_endpoints WHERE instance = 'test'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        cleanup_test_db(db_path);
    }

    /// Create a test DB with full init_db() schema
    fn setup_full_test_db() -> (HcomDb, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let temp_dir = std::env::temp_dir();
        let test_id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let db_path = temp_dir.join(format!(
            "test_hcom_full_{}_{}.db",
            std::process::id(),
            test_id
        ));

        let db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();
        (db, db_path)
    }

    #[test]
    fn test_init_db_creates_all_tables() {
        let (db, db_path) = setup_full_test_db();

        let tables: Vec<String> = db
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"events".to_string()));
        assert!(tables.contains(&"instances".to_string()));
        assert!(tables.contains(&"kv".to_string()));
        assert!(tables.contains(&"notify_endpoints".to_string()));
        assert!(tables.contains(&"process_bindings".to_string()));
        assert!(tables.contains(&"session_bindings".to_string()));

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_init_db_sets_schema_version() {
        let (db, db_path) = setup_full_test_db();

        let version: i32 = db
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_init_db_idempotent() {
        let (db, db_path) = setup_full_test_db();

        // Call init_db again - should be no-op
        db.init_db().unwrap();

        let version: i32 = db
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_init_db_creates_events_v_view() {
        let (db, db_path) = setup_full_test_db();

        // Check view exists
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='view' AND name='events_v'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_init_db_creates_fts5_table() {
        let (db, db_path) = setup_full_test_db();

        // FTS5 tables show up as 'table' in sqlite_master
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name='events_fts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(count > 0, "events_fts should exist");

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_init_db_fts_trigger_indexes_on_insert() {
        let (db, db_path) = setup_full_test_db();

        // Insert an event
        db.conn
            .execute(
                "INSERT INTO events (timestamp, type, instance, data) VALUES ('2026-01-01T00:00:00Z', 'message', 'luna', ?)",
                params![serde_json::json!({"from": "luna", "text": "hello world"}).to_string()],
            )
            .unwrap();

        // Search FTS
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM events_fts WHERE searchable MATCH 'hello'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_check_schema_compat_fresh_db() {
        let (db, db_path) = setup_full_test_db();
        match db.check_schema_compat().unwrap() {
            SchemaCompat::Ok => {} // expected
            other => panic!(
                "Expected SchemaCompat::Ok, got {:?}",
                match other {
                    SchemaCompat::NeedsArchive(r, v) => format!("NeedsArchive({}, {:?})", r, v),
                    SchemaCompat::StaleProcess => "StaleProcess".to_string(),
                    SchemaCompat::Ok => unreachable!(),
                }
            ),
        }
        cleanup_test_db(db_path);
    }

    #[test]
    fn test_ensure_schema_fresh_db() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(1000);

        let temp_dir = std::env::temp_dir();
        let test_id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let db_path = temp_dir.join(format!(
            "test_hcom_ensure_{}_{}.db",
            std::process::id(),
            test_id
        ));

        let mut db = HcomDb::open_raw(&db_path).unwrap();
        db.ensure_schema().unwrap();

        // Should have full schema
        let version: i32 = db
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_ensure_schema_archives_old_version() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(2000);

        let temp_dir = std::env::temp_dir();
        let test_id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let db_path = temp_dir.join(format!(
            "test_hcom_archive_{}_{}.db",
            std::process::id(),
            test_id
        ));

        // Create a DB with old schema version
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE events (id INTEGER PRIMARY KEY, timestamp TEXT, type TEXT, instance TEXT, data TEXT);
                 CREATE TABLE instances (name TEXT PRIMARY KEY, created_at REAL NOT NULL);
                 CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE notify_endpoints (instance TEXT, kind TEXT, port INTEGER, updated_at REAL, PRIMARY KEY(instance, kind));
                 CREATE TABLE session_bindings (session_id TEXT PRIMARY KEY, instance_name TEXT NOT NULL, created_at REAL NOT NULL);
                 PRAGMA user_version = 5;",
            )
            .unwrap();
        }

        let mut db = HcomDb::open_raw(&db_path).unwrap();
        db.ensure_schema().unwrap();

        // Should have been archived and recreated at current version
        let version: i32 = db
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // Archive directory should exist
        let archive_dir = temp_dir.join("archive");
        if archive_dir.exists() {
            let _ = std::fs::remove_dir_all(&archive_dir);
        }

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_ensure_schema_migrates_v16_to_v17_in_place() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(2500);

        let temp_dir = std::env::temp_dir();
        let test_id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let db_path = temp_dir.join(format!(
            "test_hcom_migrate_{}_{}.db",
            std::process::id(),
            test_id
        ));

        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE events (id INTEGER PRIMARY KEY, timestamp TEXT, type TEXT, instance TEXT, data TEXT);
                 CREATE TABLE instances (
                     name TEXT PRIMARY KEY,
                     tool TEXT DEFAULT 'claude',
                     created_at REAL NOT NULL,
                     launch_context TEXT DEFAULT ''
                 );
                 CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE notify_endpoints (instance TEXT, kind TEXT, port INTEGER, updated_at REAL, PRIMARY KEY(instance, kind));
                 CREATE TABLE session_bindings (session_id TEXT PRIMARY KEY, instance_name TEXT NOT NULL, created_at REAL NOT NULL);
                 PRAGMA user_version = 16;",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO instances (name, tool, created_at, launch_context) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    "luna",
                    "claude",
                    1.0f64,
                    r#"{"terminal_preset":"ghostty-tab"}"#
                ],
            )
            .unwrap();
        }

        let mut db = HcomDb::open_raw(&db_path).unwrap();
        db.ensure_schema().unwrap();

        let version: i32 = db
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        let preset: String = db
            .conn
            .query_row(
                "SELECT terminal_preset_effective FROM instances WHERE name = ?",
                params!["luna"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(preset, "ghostty-tab");
        let launch_context: String = db
            .conn
            .query_row(
                "SELECT launch_context FROM instances WHERE name = ?",
                params!["luna"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(launch_context, r#"{"terminal_preset":"ghostty-tab"}"#);

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_ensure_schema_column_guard() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(3000);

        let temp_dir = std::env::temp_dir();
        let test_id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let db_path = temp_dir.join(format!(
            "test_hcom_colguard_{}_{}.db",
            std::process::id(),
            test_id
        ));

        // Create a DB at current version but missing 'tool' column
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(&format!(
                "CREATE TABLE events (id INTEGER PRIMARY KEY, timestamp TEXT, type TEXT, instance TEXT, data TEXT);
                 CREATE TABLE instances (name TEXT PRIMARY KEY, created_at REAL NOT NULL);
                 CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE notify_endpoints (instance TEXT, kind TEXT, port INTEGER, updated_at REAL, PRIMARY KEY(instance, kind));
                 CREATE TABLE session_bindings (session_id TEXT PRIMARY KEY, instance_name TEXT NOT NULL, created_at REAL NOT NULL);
                 PRAGMA user_version = {};",
                SCHEMA_VERSION
            ))
            .unwrap();
        }

        let mut db = HcomDb::open_raw(&db_path).unwrap();

        // check_schema_compat should detect missing column
        match db.check_schema_compat().unwrap() {
            SchemaCompat::NeedsArchive(reason, _) => {
                assert!(reason.contains("instances.tool"), "reason: {}", reason);
            }
            _ => panic!("Expected NeedsArchive for missing tool column"),
        }

        // ensure_schema should fix it
        db.ensure_schema().unwrap();

        let version: i32 = db
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        cleanup_test_db(db_path);
    }

    /// Regression test for issue #16: init_db() stamped user_version=17 without
    /// actually adding the terminal_preset_* columns. ensure_schema must repair
    /// this via migration instead of archiving (which would lose data).
    #[test]
    fn test_ensure_schema_repairs_stamped_but_not_migrated_db() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(4000);

        let temp_dir = std::env::temp_dir();
        let test_id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let db_path = temp_dir.join(format!(
            "test_hcom_repair_{}_{}.db",
            std::process::id(),
            test_id
        ));

        // Simulate the bug: create a v16-style DB but stamp it as v17
        // (this is what init_db() did — CREATE IF NOT EXISTS is a no-op on
        // existing tables, then it unconditionally set user_version = 17)
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE events (id INTEGER PRIMARY KEY AUTOINCREMENT, timestamp TEXT NOT NULL, type TEXT NOT NULL, instance TEXT NOT NULL, data TEXT NOT NULL);
                 CREATE TABLE instances (
                     name TEXT PRIMARY KEY,
                     session_id TEXT UNIQUE,
                     parent_session_id TEXT,
                     parent_name TEXT,
                     tag TEXT,
                     last_event_id INTEGER DEFAULT 0,
                     status TEXT DEFAULT 'active',
                     status_time INTEGER DEFAULT 0,
                     status_context TEXT DEFAULT '',
                     status_detail TEXT DEFAULT '',
                     last_stop INTEGER DEFAULT 0,
                     directory TEXT,
                     created_at REAL NOT NULL,
                     transcript_path TEXT DEFAULT '',
                     tcp_mode INTEGER DEFAULT 0,
                     wait_timeout INTEGER DEFAULT 86400,
                     background INTEGER DEFAULT 0,
                     background_log_file TEXT DEFAULT '',
                     name_announced INTEGER DEFAULT 0,
                     agent_id TEXT UNIQUE,
                     running_tasks TEXT DEFAULT '',
                     origin_device_id TEXT DEFAULT '',
                     hints TEXT DEFAULT '',
                     subagent_timeout INTEGER,
                     tool TEXT DEFAULT 'claude',
                     launch_args TEXT DEFAULT '',
                     idle_since TEXT DEFAULT '',
                     pid INTEGER DEFAULT NULL,
                     launch_context TEXT DEFAULT ''
                 );
                 CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE notify_endpoints (instance TEXT NOT NULL, kind TEXT NOT NULL, port INTEGER NOT NULL, updated_at REAL NOT NULL, PRIMARY KEY(instance, kind));
                 CREATE TABLE session_bindings (session_id TEXT PRIMARY KEY, instance_name TEXT NOT NULL, created_at REAL NOT NULL);
                 CREATE TABLE process_bindings (process_id TEXT PRIMARY KEY, session_id TEXT, instance_name TEXT, updated_at REAL NOT NULL);
                 PRAGMA user_version = 17;",
            )
            .unwrap();
            // Insert test data that should survive the repair
            conn.execute(
                "INSERT INTO instances (name, tool, created_at) VALUES ('luna', 'claude', 1.0)",
                [],
            )
            .unwrap();
        }

        // Verify columns are missing before repair
        {
            let conn = Connection::open(&db_path).unwrap();
            let cols: Vec<String> = conn
                .prepare("PRAGMA table_info(instances)")
                .unwrap()
                .query_map([], |row| row.get::<_, String>(1))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect();
            assert!(
                !cols.contains(&"terminal_preset_requested".to_string()),
                "column should be missing before repair"
            );
        }

        let mut db = HcomDb::open_raw(&db_path).unwrap();
        db.ensure_schema().unwrap();

        // Should be at current version
        let version: i32 = db
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // Columns should now exist
        let cols: Vec<String> = db
            .conn
            .prepare("PRAGMA table_info(instances)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(
            cols.contains(&"terminal_preset_requested".to_string()),
            "terminal_preset_requested column should exist after repair"
        );
        assert!(
            cols.contains(&"terminal_preset_effective".to_string()),
            "terminal_preset_effective column should exist after repair"
        );

        // Test data should have survived (not archived)
        let name: String = db
            .conn
            .query_row(
                "SELECT name FROM instances WHERE name = 'luna'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(name, "luna");

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_kv_get_set() {
        let (db, db_path) = setup_full_test_db();

        // Get non-existent key
        assert!(db.kv_get("foo").unwrap().is_none());

        // Set and get
        db.kv_set("foo", Some("bar")).unwrap();
        assert_eq!(db.kv_get("foo").unwrap(), Some("bar".to_string()));

        // Overwrite
        db.kv_set("foo", Some("baz")).unwrap();
        assert_eq!(db.kv_get("foo").unwrap(), Some("baz".to_string()));

        // Delete
        db.kv_set("foo", None).unwrap();
        assert!(db.kv_get("foo").unwrap().is_none());

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_kv_prefix() {
        let (db, db_path) = setup_full_test_db();

        db.kv_set("events_sub:1", Some("val1")).unwrap();
        db.kv_set("events_sub:2", Some("val2")).unwrap();
        db.kv_set("other:1", Some("val3")).unwrap();

        let results = db.kv_prefix("events_sub:").unwrap();
        assert_eq!(results.len(), 2);

        // Wildcards in prefix should be escaped — not treated as LIKE patterns
        db.kv_set("100%_done", Some("yes")).unwrap();
        db.kv_set("100x_done", Some("no")).unwrap();
        let results = db.kv_prefix("100%").unwrap();
        assert_eq!(results.len(), 1, "% in prefix must be escaped");
        assert_eq!(results[0].0, "100%_done");

        let results = db.kv_prefix("events_sub").unwrap();
        // underscore in "events_sub" should match literally, not as single-char wildcard
        assert_eq!(results.len(), 2, "_ in prefix must be escaped");

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_session_binding_crud() {
        let (db, db_path) = setup_full_test_db();

        // Create instance first (FK constraint)
        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0)",
                [],
            )
            .unwrap();

        // No binding initially
        assert!(db.get_session_binding("sess-1").unwrap().is_none());

        // Set binding
        db.set_session_binding("sess-1", "luna").unwrap();
        assert_eq!(
            db.get_session_binding("sess-1").unwrap(),
            Some("luna".to_string())
        );

        // has_session_binding
        assert!(db.has_session_binding("luna"));

        // Delete binding
        db.delete_session_binding("sess-1").unwrap();
        assert!(db.get_session_binding("sess-1").unwrap().is_none());
        assert!(!db.has_session_binding("luna"));

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_session_binding_conflict() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('nova', 1000.0)",
                [],
            )
            .unwrap();

        // Bind session to luna
        db.set_session_binding("sess-1", "luna").unwrap();

        // Try binding same session to nova - should fail
        let result = db.set_session_binding("sess-1", "nova");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("already bound to luna")
        );

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_rebind_session() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, session_id, created_at) VALUES ('luna', 'sess-1', 1000.0)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('nova', 1000.0)",
                [],
            )
            .unwrap();

        // Bind to luna first
        db.set_session_binding("sess-1", "luna").unwrap();

        // Rebind to nova (should clear from luna)
        db.rebind_session("sess-1", "nova").unwrap();
        assert_eq!(
            db.get_session_binding("sess-1").unwrap(),
            Some("nova".to_string())
        );

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_rebind_instance_session() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0)",
                [],
            )
            .unwrap();

        db.rebind_instance_session("luna", "sess-new").unwrap();
        assert_eq!(
            db.get_session_binding("sess-new").unwrap(),
            Some("luna".to_string())
        );

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_log_event_returns_id() {
        let (db, db_path) = setup_full_test_db();

        let data = serde_json::json!({"status": "active", "context": "test"});
        let id1 = db.log_event("status", "luna", &data).unwrap();
        let id2 = db.log_event("status", "luna", &data).unwrap();

        assert!(id1 > 0);
        assert_eq!(id2, id1 + 1);

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_store_launch_context_merges_late_pty_metadata() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, tool, created_at, launch_context) VALUES (?1, ?2, ?3, ?4)",
                params![
                    "luna",
                    "codex",
                    1.0f64,
                    r#"{"process_id":"proc-1","terminal_preset_effective":"kitty-split","terminal_preset":"kitty-split"}"#
                ],
            )
            .unwrap();

        db.store_launch_context(
            "luna",
            r#"{"process_id":"proc-2","kitty_listen_on":"unix:/tmp/kitty","terminal_id":"11","pane_id":"11"}"#,
        )
        .unwrap();

        let launch_context: String = db
            .conn
            .query_row(
                "SELECT launch_context FROM instances WHERE name = ?",
                params!["luna"],
                |row| row.get(0),
            )
            .unwrap();
        let launch_context: serde_json::Value = serde_json::from_str(&launch_context).unwrap();

        assert_eq!(launch_context["process_id"], "proc-1");
        assert_eq!(launch_context["terminal_preset_effective"], "kitty-split");
        assert_eq!(launch_context["terminal_preset"], "kitty-split");
        assert_eq!(launch_context["kitty_listen_on"], "unix:/tmp/kitty");
        assert_eq!(launch_context["terminal_id"], "11");
        assert_eq!(launch_context["pane_id"], "11");

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_get_events_since() {
        let (db, db_path) = setup_full_test_db();

        let data1 = serde_json::json!({"status": "active"});
        let data2 = serde_json::json!({"action": "ready"});
        let id1 = db.log_event("status", "luna", &data1).unwrap();
        let _id2 = db.log_event("life", "nova", &data2).unwrap();

        // Get all events
        let all = db.get_events_since(0, None, None).unwrap();
        assert_eq!(all.len(), 2);

        // Get events since first
        let since = db.get_events_since(id1, None, None).unwrap();
        assert_eq!(since.len(), 1);

        // Filter by type
        let status_only = db.get_events_since(0, Some("status"), None).unwrap();
        assert_eq!(status_only.len(), 1);

        // Filter by instance
        let nova_only = db.get_events_since(0, None, Some("nova")).unwrap();
        assert_eq!(nova_only.len(), 1);

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_get_last_event_id() {
        let (db, db_path) = setup_full_test_db();

        assert_eq!(db.get_last_event_id(), 0);

        let data = serde_json::json!({"status": "active"});
        let id = db.log_event("status", "luna", &data).unwrap();
        assert_eq!(db.get_last_event_id(), id);

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_notify_batch_failure_is_targeted_and_deduplicated() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('leku', 1000.0)",
                [],
            )
            .unwrap();

        db.notify_batch_failure("leku", "batch-1", "para", "boom")
            .unwrap();
        db.notify_batch_failure("leku", "batch-1", "para", "boom")
            .unwrap();

        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM events
                 WHERE type = 'message'
                   AND instance = 'sys_[hcom-launcher]'
                   AND json_extract(data, '$.text') = '@leku Launch failed: para: boom (batch: batch-1)'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_save_and_get_instance() {
        let (db, db_path) = setup_full_test_db();

        let mut data = std::collections::HashMap::new();
        data.insert("name".to_string(), serde_json::json!("luna"));
        data.insert("tool".to_string(), serde_json::json!("claude"));
        data.insert("created_at".to_string(), serde_json::json!(1000.0));
        data.insert("status".to_string(), serde_json::json!("active"));

        db.save_instance(&data).unwrap();

        let inst = db.get_instance("luna").unwrap().unwrap();
        assert_eq!(inst["name"], "luna");
        assert_eq!(inst["tool"], "claude");
        assert_eq!(inst["status"], "active");

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_update_instance() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, status, created_at) VALUES ('luna', 'active', 1000.0)",
                [],
            )
            .unwrap();

        let mut updates = std::collections::HashMap::new();
        updates.insert("status".to_string(), serde_json::json!("listening"));
        updates.insert("tag".to_string(), serde_json::json!("api"));

        db.update_instance("luna", &updates).unwrap();

        let inst = db.get_instance("luna").unwrap().unwrap();
        assert_eq!(inst["status"], "listening");
        assert_eq!(inst["tag"], "api");

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_iter_instances() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 2000.0)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('nova', 1000.0)",
                [],
            )
            .unwrap();

        let instances = db.iter_instances().unwrap();
        assert_eq!(instances.len(), 2);
        // Should be ordered by created_at DESC
        assert_eq!(instances[0]["name"], "luna");
        assert_eq!(instances[1]["name"], "nova");

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_process_binding_crud() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0)",
                [],
            )
            .unwrap();

        // Set process binding
        db.set_process_binding("pid-123", "sess-1", "luna").unwrap();
        assert!(db.has_process_binding_for_instance("luna"));

        // Get binding
        let name = db.get_process_binding("pid-123").unwrap();
        assert_eq!(name, Some("luna".to_string()));

        // Delete
        db.delete_process_binding("pid-123").unwrap();
        assert!(!db.has_process_binding_for_instance("luna"));

        cleanup_test_db(db_path);
    }

    #[test]
    fn test_delete_process_bindings_for_instance() {
        let (db, db_path) = setup_full_test_db();

        db.conn
            .execute(
                "INSERT INTO instances (name, created_at) VALUES ('luna', 1000.0)",
                [],
            )
            .unwrap();

        db.set_process_binding("pid-1", "sess-1", "luna").unwrap();
        db.set_process_binding("pid-2", "sess-2", "luna").unwrap();
        assert!(db.has_process_binding_for_instance("luna"));

        db.delete_process_bindings_for_instance("luna").unwrap();
        assert!(!db.has_process_binding_for_instance("luna"));

        cleanup_test_db(db_path);
    }

    /// Regression: a broadcast must NOT count as direct unread for a dormant
    /// subagent, otherwise SubagentStop wakes every dormant subagent on every
    /// broadcast and the "no message in → no keep-alive" gate is broken.
    #[test]
    fn test_has_direct_unread_ignores_broadcasts() {
        let (db, db_path) = setup_full_test_db();
        db.conn
            .execute(
                "INSERT INTO instances (name, created_at, last_event_id) \
                 VALUES ('luna_reviewer_1', 1000.0, 0)",
                [],
            )
            .unwrap();

        // Broadcast to everyone — must be ignored.
        db.log_event(
            "message",
            "sender",
            &serde_json::json!({"scope": "broadcast", "from": "sender", "text": "hi all"}),
        )
        .unwrap();
        assert!(!db.has_direct_unread("luna_reviewer_1"));

        // Direct mention of a different subagent — also ignored.
        db.log_event(
            "message",
            "sender",
            &serde_json::json!({
                "scope": "mentions",
                "mentions": ["other"],
                "from": "sender",
                "text": "hey other",
            }),
        )
        .unwrap();
        assert!(!db.has_direct_unread("luna_reviewer_1"));

        // Direct mention of this subagent — must trigger.
        db.log_event(
            "message",
            "sender",
            &serde_json::json!({
                "scope": "mentions",
                "mentions": ["luna_reviewer_1"],
                "from": "sender",
                "text": "hey you",
            }),
        )
        .unwrap();
        assert!(db.has_direct_unread("luna_reviewer_1"));

        cleanup_test_db(db_path);
    }
}
