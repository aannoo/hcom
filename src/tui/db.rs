//! Real database-backed DataSource for the TUI.
//!
//! Reads from ~/.hcom/hcom.db (or HCOM_DIR/hcom.db) to populate DataState.
//! Used when HCOM_MOCK_TUI is not set.

use rusqlite::{Connection, params};
use std::path::PathBuf;

use crate::log::log_warn;
use crate::tui::app::DataState;
use crate::tui::data::DataSource;
use crate::tui::model::*;
use crate::tui::status;

use crate::paths;
use crate::shared::ST_ACTIVE;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

/// Local device UUID from kv table.
fn read_device_uuid(conn: &Connection) -> String {
    kv_get(conn, "device_uuid").unwrap_or_default()
}

pub struct DbDataSource {
    db_path: PathBuf,
    conn: Option<Connection>,
    last_data_version: u64,
    cached: Option<DataState>,
    last_error: Option<String>,
    config_mtime: Option<std::time::SystemTime>,
    timeline_limit: usize,
}

impl Default for DbDataSource {
    fn default() -> Self {
        Self::new()
    }
}

impl DbDataSource {
    pub fn new() -> Self {
        Self {
            db_path: paths::db_path(),
            conn: None,
            last_data_version: 0,
            cached: None,
            last_error: None,
            config_mtime: None,
            timeline_limit: 200,
        }
    }

    /// Lazy-open persistent connection; reconnects on failure.
    fn ensure_conn(&mut self) -> Option<&Connection> {
        if self.conn.is_none() {
            let conn = match Connection::open(&self.db_path) {
                Ok(c) => c,
                Err(e) => {
                    self.last_error = Some(format!("open {}: {}", self.db_path.display(), e));
                    return None;
                }
            };
            // query_only=ON: TUI is read-only; any accidental write will
            // error immediately rather than silently succeed.
            if let Err(e) = conn.execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA busy_timeout=3000; PRAGMA query_only=ON;",
            ) {
                self.last_error = Some(format!(
                    "init database pragmas {}: {}",
                    self.db_path.display(),
                    e
                ));
                return None;
            }
            self.conn = Some(conn);
            // Force full reload on new connection
            self.last_data_version = 0;
            self.cached = None;
            self.last_error = None;
        }
        self.conn.as_ref()
    }

    /// Check PRAGMA data_version and config.toml mtime for changes.
    fn data_version_changed(&self) -> bool {
        let conn = match &self.conn {
            Some(c) => c,
            None => return true,
        };
        let version = conn
            .query_row("PRAGMA data_version", [], |row| row.get::<_, i64>(0))
            .ok()
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(0);
        if version != self.last_data_version {
            return true;
        }
        // Also check config.toml mtime (relay_enabled lives there, not in SQLite)
        config_toml_mtime() != self.config_mtime
    }

    /// Run all queries and update cache.
    fn full_load(&mut self) -> DataState {
        let conn = match &self.conn {
            Some(c) => c,
            None => return DataState::empty(),
        };

        // Update cached data_version + config mtime
        self.last_data_version = conn
            .query_row("PRAGMA data_version", [], |row| row.get::<_, i64>(0))
            .ok()
            .and_then(|v| u64::try_from(v).ok())
            .unwrap_or(0);
        self.config_mtime = config_toml_mtime();

        let data = load_all(conn, self.timeline_limit);
        self.cached = Some(data.clone());
        data
    }
}

impl DataSource for DbDataSource {
    fn set_timeline_limit(&mut self, limit: usize) {
        self.timeline_limit = limit;
    }

    fn last_error(&self) -> Option<String> {
        self.last_error.clone()
    }

    fn load_all_stopped(&mut self) -> Vec<Agent> {
        let conn = match self.ensure_conn() {
            Some(c) => c,
            None => return vec![],
        };
        load_stopped(conn, epoch_now(), None)
    }

    fn load_if_changed(&mut self) -> Option<DataState> {
        // Ensure we have a connection (lazy open / reconnect)
        if self.ensure_conn().is_none() {
            self.cached = Some(DataState::empty());
            return self.cached.clone();
        }

        // Fast path: DB unchanged
        if !self.data_version_changed() {
            return None;
        }

        Some(self.full_load())
    }

    fn load(&mut self) -> DataState {
        if let Some(data) = self.load_if_changed() {
            return data;
        }
        self.cached.clone().unwrap_or_else(DataState::empty)
    }

    fn search_timeline(&mut self, query: &str, limit: usize) -> (Vec<Message>, Vec<Event>) {
        if self.ensure_conn().is_none() {
            return (vec![], vec![]);
        }
        match &self.conn {
            Some(conn) => load_timeline_search(conn, query, limit),
            None => (vec![], vec![]),
        }
    }
}

/// Run all snapshot queries against the connection.
fn load_all(conn: &Connection, default_limit: usize) -> DataState {
    let device_uuid = read_device_uuid(conn);
    let now = epoch_now();

    // Load all instances
    let (mut agents, mut remote_agents) = load_instances(conn, &device_uuid, now);

    // Compute unread counts
    compute_unread_batch(conn, &mut agents);

    // Load recently stopped (last 10 minutes from events)
    let stopped_agents = load_recently_stopped(conn, now);

    // Load orphan processes from pidtrack file
    let orphans = load_orphans(conn);

    // Load one shared timeline window, then split into message vs status/life
    // so both panes stay aligned to the same event-id/time range.
    let timeline_limit = env_usize("HCOM_TUI_TIMELINE_LIMIT", default_limit);
    let (mut messages, mut events) = load_timeline(conn, timeline_limit);
    messages.sort_by(|a, b| a.time.total_cmp(&b.time));
    events.sort_by(|a, b| a.time.total_cmp(&b.time));

    // Read relay config flags once per snapshot — both relay_enabled and
    // load_relay_state need them, so reading config.toml twice would be
    // pointless I/O on the TUI's hot path.
    let (configured, enabled) = read_relay_config_flags();
    let relay_enabled = configured && enabled;

    // Sort agents by created_at descending (newest first)
    agents.sort_by(|a, b| b.created_at.total_cmp(&a.created_at));
    remote_agents.sort_by(|a, b| b.created_at.total_cmp(&a.created_at));

    let relay_health = load_relay_state(conn, configured, enabled);

    DataState {
        agents,
        remote_agents,
        stopped_agents,
        orphans,
        messages,
        events,
        relay_enabled,
        relay_health,
        search_results: None,
    }
}

// ── Instance loading ────────────────────────────────────────────

/// Extract a string field from a JSON object, returning `default` if missing or non-string.
fn json_str<'a>(v: &'a serde_json::Value, key: &str, default: &'a str) -> &'a str {
    v.get(key).and_then(|v| v.as_str()).unwrap_or(default)
}

fn parse_tool(s: &str) -> Tool {
    match s {
        "claude" => Tool::Claude,
        "gemini" => Tool::Gemini,
        "codex" => Tool::Codex,
        "opencode" => Tool::OpenCode,
        _ => Tool::Adhoc,
    }
}

fn parse_status(s: &str) -> Option<AgentStatus> {
    match s {
        "active" => Some(AgentStatus::Active),
        "listening" => Some(AgentStatus::Listening),
        "blocked" => Some(AgentStatus::Blocked),
        "launching" => Some(AgentStatus::Launching),
        "inactive" => Some(AgentStatus::Inactive),
        _ => None,
    }
}

fn load_instances(conn: &Connection, device_uuid: &str, now: f64) -> (Vec<Agent>, Vec<Agent>) {
    let mut local = Vec::new();
    let mut remote = Vec::new();

    let mut stmt = match conn.prepare(
        "SELECT name, tool, status, status_context, status_detail,
                created_at, status_time, last_stop, tcp_mode,
                directory, tag, last_event_id, origin_device_id,
                pid, session_id, background, terminal_preset_effective
         FROM instances ORDER BY created_at DESC",
    ) {
        Ok(s) => s,
        Err(_) => return (local, remote),
    };

    let rows = match stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,          // name (PK, never NULL)
            row.get::<_, String>(1)?,          // tool (never NULL)
            row.get::<_, String>(2)?,          // status (never NULL)
            row.get::<_, Option<String>>(3)?,  // status_context
            row.get::<_, Option<String>>(4)?,  // status_detail
            row.get::<_, f64>(5)?,             // created_at (never NULL)
            row.get::<_, Option<i64>>(6)?,     // status_time
            row.get::<_, Option<f64>>(7)?,     // last_stop (heartbeat, may be real or int)
            row.get::<_, Option<i64>>(8)?,     // tcp_mode
            row.get::<_, Option<String>>(9)?,  // directory
            row.get::<_, Option<String>>(10)?, // tag
            row.get::<_, Option<i64>>(11)?,    // last_event_id
            row.get::<_, Option<String>>(12)?, // origin_device_id
            row.get::<_, Option<i64>>(13)?,    // pid
            row.get::<_, Option<String>>(14)?, // session_id
            row.get::<_, Option<i64>>(15)?,    // background (headless)
            row.get::<_, Option<String>>(16)?, // terminal_preset_effective
        ))
    }) {
        Ok(r) => r,
        Err(_) => return (local, remote),
    };

    for row in rows {
        let row = match row {
            Ok(r) => r,
            Err(e) => {
                log_warn("tui", "db.instance_parse_error", &format!("{}", e));
                continue;
            }
        };
        let (
            name,
            tool_s,
            status_s,
            ctx,
            detail,
            created_at,
            status_time,
            last_stop,
            tcp_mode,
            directory,
            tag,
            last_event_id,
            origin_device_id,
            pid,
            session_id,
            background,
            terminal_preset,
        ) = row;

        let ctx = ctx.unwrap_or_default();
        let detail = detail.unwrap_or_default();
        let status_time = status_time.unwrap_or(0);
        let last_stop = last_stop.unwrap_or(0.0);
        let tcp_mode = tcp_mode.unwrap_or(0);
        let directory = directory.unwrap_or_default();
        let origin_device_id = origin_device_id.unwrap_or_default();
        let background = background.unwrap_or(0);

        let is_remote = !origin_device_id.is_empty() && origin_device_id != device_uuid;

        // Compute heartbeat: last_stop, fallback to status_time, fallback to created_at
        let heartbeat_epoch = if last_stop > 0.0 {
            last_stop
        } else if status_time > 0 {
            status_time as f64
        } else {
            created_at
        };

        let has_tcp = tcp_mode != 0;

        // Device name for remote agents (short suffix from device UUID)
        let device_name = if is_remote {
            Some(
                origin_device_id
                    .chars()
                    .take(4)
                    .collect::<String>()
                    .to_uppercase(),
            )
        } else {
            None
        };

        // Sync age for remote agents from KV
        let sync_age = if is_remote {
            get_device_sync_age(conn, &origin_device_id, now)
        } else {
            None
        };

        let terminal_preset = terminal_preset.filter(|s| !s.is_empty() && s != "default");

        // Build agent with raw DB values first.
        // Unknown statuses are treated as inactive to avoid falsely showing them as live.
        let parsed_status = parse_status(&status_s);
        let mut agent = Agent {
            name,
            tool: parse_tool(&tool_s),
            status: parsed_status.unwrap_or(AgentStatus::Inactive),
            status_context: ctx,
            status_detail: detail,
            created_at,
            status_time: status_time as f64,
            last_heartbeat: heartbeat_epoch,
            has_tcp,
            directory,
            tag: tag.unwrap_or_default(),
            unread: 0, // computed separately
            last_event_id: last_event_id.map(|id| id as u64),
            device_name,
            sync_age,
            pid: pid.map(|p| p as u32),
            session_id: session_id.filter(|s| !s.is_empty()),
            headless: background != 0,
            terminal_preset,
        };

        if parsed_status.is_none() {
            if agent.status_detail.is_empty() {
                agent.status_detail = format!("unknown status '{}'", status_s);
            }
            if agent.status_context.is_empty() {
                agent.status_context = "unknown_status".into();
            }
        }

        // Apply stale detection for local agents
        if !is_remote {
            let computed = status::compute_status(&agent, now, None);
            agent.status = computed.status;
            agent.status_context = computed.status_context;
            agent.status_detail = computed.status_detail;
        }

        if is_remote {
            remote.push(agent);
        } else {
            local.push(agent);
        }
    }

    (local, remote)
}

fn get_device_sync_age(conn: &Connection, device_id: &str, now: f64) -> Option<String> {
    let key = format!("relay_sync_time_{}", device_id);
    let ts: f64 = conn
        .query_row("SELECT value FROM kv WHERE key = ?", params![key], |row| {
            row.get::<_, String>(0)
        })
        .ok()
        .and_then(|s| s.parse::<f64>().ok())?;

    let age = (now - ts).max(0.0) as u64;
    Some(format!("{} ago", format_duration_short(age)))
}

// ── Unread counts ───────────────────────────────────────────────

fn compute_unread_batch(conn: &Connection, agents: &mut [Agent]) {
    if agents.is_empty() {
        return;
    }

    // Find minimum waterline
    let min_waterline = agents
        .iter()
        .filter_map(|a| a.last_event_id)
        .min()
        .unwrap_or(0);

    // Fetch all messages since minimum waterline
    let mut stmt = match conn
        .prepare("SELECT id, data FROM events WHERE id > ? AND type = 'message' ORDER BY id")
    {
        Ok(s) => s,
        Err(_) => return,
    };

    let msgs: Vec<(u64, String)> = stmt
        .query_map(params![min_waterline as i64], |row| {
            Ok((row.get::<_, i64>(0)? as u64, row.get::<_, String>(1)?))
        })
        .ok()
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default();

    let mut broadcast_ids: Vec<u64> = Vec::new();
    let mut broadcast_by_sender: std::collections::HashMap<String, Vec<u64>> =
        std::collections::HashMap::new();
    let mut mentions_by_recipient: std::collections::HashMap<String, Vec<u64>> =
        std::collections::HashMap::new();

    for (event_id, data) in &msgs {
        let json = match serde_json::from_str::<serde_json::Value>(data) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let sender_kind = json_str(&json, "sender_kind", "agent");
        if sender_kind == "system" || sender_kind == "sys" {
            continue;
        }

        let from = json_str(&json, "from", "");
        let scope = json_str(&json, "scope", "broadcast");

        match scope {
            "broadcast" => {
                broadcast_ids.push(*event_id);
                broadcast_by_sender
                    .entry(from.to_string())
                    .or_default()
                    .push(*event_id);
            }
            "mentions" => {
                if let Some(arr) = json.get("mentions").and_then(|m| m.as_array()) {
                    for recipient in arr.iter().filter_map(|v| v.as_str()) {
                        if recipient == from {
                            continue;
                        }
                        mentions_by_recipient
                            .entry(recipient.to_string())
                            .or_default()
                            .push(*event_id);
                    }
                }
            }
            _ => {}
        }
    }

    for agent in agents.iter_mut() {
        let waterline = agent.last_event_id.unwrap_or(0);
        let mut count = count_gt(&broadcast_ids, waterline);
        if let Some(self_sent) = broadcast_by_sender.get(&agent.name) {
            count = count.saturating_sub(count_gt(self_sent, waterline));
        }
        if let Some(mentions) = mentions_by_recipient.get(&agent.name) {
            count += count_gt(mentions, waterline);
        }
        agent.unread = count;
    }
}

fn count_gt(sorted_ids: &[u64], waterline: u64) -> usize {
    let idx = sorted_ids.partition_point(|id| *id <= waterline);
    sorted_ids.len().saturating_sub(idx)
}

// ── Recently stopped ────────────────────────────────────────────

fn load_recently_stopped(conn: &Connection, now: f64) -> Vec<Agent> {
    load_stopped(
        conn,
        now,
        Some(crate::instance_lifecycle::RECENTLY_STOPPED_WINDOW),
    )
}

/// Load stopped agents. `max_age_secs`: None = all time, Some(n) = last n seconds.
fn load_stopped(conn: &Connection, now: f64, max_age_secs: Option<f64>) -> Vec<Agent> {
    let cutoff = max_age_secs
        .map(|age| (now - age).max(0.0) as i64)
        .unwrap_or(0);
    let row_limit = 512i64;
    let mut stmt = match conn.prepare(
        "SELECT instance, data, timestamp FROM events
         WHERE type = 'life'
           AND json_extract(data, '$.action') IN ('stopped', 'killed')
           AND unixepoch(timestamp) >= ?
         ORDER BY id DESC
         LIMIT ?",
    ) {
        Ok(s) => s,
        Err(_) => return vec![],
    };

    let rows: Vec<(String, String, String)> = stmt
        .query_map(params![cutoff, row_limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .ok()
        .map(|r| r.flatten().collect())
        .unwrap_or_default();

    // Fetch all active instance names in one query
    let active_names: std::collections::HashSet<String> = conn
        .prepare("SELECT name FROM instances")
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, String>(0))
                .ok()
                .map(|r| r.flatten().collect())
        })
        .unwrap_or_default();

    // Deduplicate by name (keep most recent)
    let mut seen = std::collections::HashSet::new();
    let mut stopped = Vec::new();

    for (name, data, ts) in &rows {
        if parse_iso_to_epoch(ts) < cutoff as f64 {
            continue;
        }
        if !seen.insert(name.clone()) {
            continue;
        }
        if active_names.contains(name) {
            continue;
        }

        let json: serde_json::Value = serde_json::from_str(data).unwrap_or_default();
        let action = json_str(&json, "action", "stopped");
        let snapshot = json.get("snapshot").cloned().unwrap_or_default();

        let tool_s = json_str(&snapshot, "tool", "claude");
        let directory = json_str(&snapshot, "directory", "");
        let tag = json_str(&snapshot, "tag", "");
        let created_at = snapshot
            .get("created_at")
            .and_then(|v| {
                v.as_f64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(now);

        stopped.push(Agent {
            name: name.clone(),
            tool: parse_tool(tool_s),
            status: AgentStatus::Inactive,
            status_context: action.to_string(),
            status_detail: String::new(),
            created_at,
            status_time: now,
            last_heartbeat: now,
            has_tcp: false,
            directory: directory.to_string(),
            tag: tag.to_string(),
            unread: 0,
            last_event_id: None,
            device_name: None,
            sync_age: None,
            headless: false,
            session_id: None,
            pid: None,
            terminal_preset: None,
        });
    }

    stopped
}

// ── Orphan processes ────────────────────────────────────────────

/// 5-second TTL cache for pidtrack data to avoid excessive I/O in TUI polling.
static ORPHAN_CACHE: std::sync::Mutex<Option<(std::time::Instant, Vec<OrphanProcess>)>> =
    std::sync::Mutex::new(None);
const ORPHAN_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(5);

fn load_orphans(conn: &Connection) -> Vec<OrphanProcess> {
    // Check cache first
    if let Ok(guard) = ORPHAN_CACHE.lock() {
        if let Some((ts, ref cached)) = *guard {
            if ts.elapsed() < ORPHAN_CACHE_TTL {
                // Still need to filter by active DB PIDs
                let active_db_pids: Vec<u32> = conn
                    .prepare("SELECT pid FROM instances WHERE pid IS NOT NULL")
                    .ok()
                    .and_then(|mut stmt| {
                        stmt.query_map([], |row| row.get::<_, i64>(0))
                            .ok()
                            .map(|rows| rows.flatten().map(|p| p as u32).collect())
                    })
                    .unwrap_or_default();
                return cached
                    .iter()
                    .filter(|o| !active_db_pids.contains(&o.pid))
                    .cloned()
                    .collect();
            }
        }
    }

    let path = paths::pidtrack_path();
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let pidmap: std::collections::HashMap<String, serde_json::Value> =
        match serde_json::from_str(&content) {
            Ok(m) => m,
            Err(_) => return vec![],
        };

    // Get active instance PIDs from DB
    let active_db_pids: Vec<u32> = conn
        .prepare("SELECT pid FROM instances WHERE pid IS NOT NULL")
        .ok()
        .and_then(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, i64>(0))
                .ok()
                .map(|rows| rows.flatten().map(|p| p as u32).collect())
        })
        .unwrap_or_default();

    // Build all alive orphans (before active_pids filter) for caching
    let mut all_alive = Vec::new();
    for (pid_str, info) in &pidmap {
        let pid: u32 = match pid_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Check if PID is still alive
        if !crate::pidtrack::is_alive(pid) {
            continue;
        }

        let tool_s = json_str(info, "tool", "claude");
        let names: Vec<String> = info
            .get("names")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let launched_at = info
            .get("launched_at")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let directory = json_str(info, "directory", "").to_string();

        all_alive.push(OrphanProcess {
            pid,
            tool: parse_tool(tool_s),
            names,
            launched_at,
            directory,
        });
    }

    // Update cache with all alive processes
    if let Ok(mut guard) = ORPHAN_CACHE.lock() {
        *guard = Some((std::time::Instant::now(), all_alive.clone()));
    }

    // Filter out active DB PIDs for return
    all_alive.retain(|o| !active_db_pids.contains(&o.pid));
    all_alive
}

// ── Timeline ────────────────────────────────────────────────────

/// Parse timeline rows into messages and events.
fn parse_timeline_rows(
    rows: Vec<(i64, String, String, String, String)>,
) -> (Vec<Message>, Vec<Event>) {
    let mut messages = Vec::new();
    let mut events = Vec::new();
    for (id, timestamp, instance, event_type, data) in rows {
        match event_type.as_str() {
            "message" => {
                if let Some(msg) = parse_message_row(id, &timestamp, &data) {
                    messages.push(msg);
                }
            }
            "status" | "life" => {
                if let Some(event) =
                    parse_status_or_life_row(id, &timestamp, &instance, &event_type, &data)
                {
                    events.push(event);
                }
            }
            _ => {}
        }
    }
    (messages, events)
}

/// Query 5-column timeline rows from a prepared statement.
fn query_timeline_rows(
    stmt: &mut rusqlite::Statement,
    params: &[&dyn rusqlite::types::ToSql],
) -> Vec<(i64, String, String, String, String)> {
    stmt.query_map(params, |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
        ))
    })
    .ok()
    .map(|r| r.flatten().collect())
    .unwrap_or_default()
}

fn load_timeline(conn: &Connection, limit: usize) -> (Vec<Message>, Vec<Event>) {
    let mut stmt = match conn.prepare(
        "SELECT id, timestamp, instance, type, data FROM events
         WHERE type IN ('message', 'status', 'life')
         ORDER BY id DESC LIMIT ?",
    ) {
        Ok(s) => s,
        Err(_) => return (vec![], vec![]),
    };
    let limit_param = limit as i64;
    parse_timeline_rows(query_timeline_rows(&mut stmt, &[&limit_param]))
}

fn load_timeline_search(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> (Vec<Message>, Vec<Event>) {
    // Strip double-quotes and wrap in quotes for FTS5 phrase search.
    // Empty after stripping (e.g. query was all quotes) → return empty; `""` is invalid FTS5.
    let stripped = query.replace('"', "");
    if stripped.is_empty() {
        return (vec![], vec![]);
    }
    let fts_query = format!("\"{}\"", stripped);
    let mut stmt = match conn.prepare(
        "SELECT e.id, e.timestamp, e.instance, e.type, e.data
         FROM events e
         JOIN events_fts ON events_fts.rowid = e.id
         WHERE events_fts MATCH ?
           AND e.type IN ('message', 'status', 'life')
         ORDER BY e.id DESC LIMIT ?",
    ) {
        Ok(s) => s,
        Err(_) => return (vec![], vec![]),
    };
    let limit_param = limit as i64;
    let (mut messages, mut events) = parse_timeline_rows(query_timeline_rows(
        &mut stmt,
        &[&fts_query as &dyn rusqlite::types::ToSql, &limit_param],
    ));

    // Sort ascending by time (same order as load_timeline results in load_all)
    messages.sort_by(|a, b| a.time.total_cmp(&b.time));
    events.sort_by(|a, b| a.time.total_cmp(&b.time));

    (messages, events)
}

fn parse_message_row(id: i64, timestamp: &str, data: &str) -> Option<Message> {
    let json: serde_json::Value = serde_json::from_str(data).ok()?;

    let from = json_str(&json, "from", "?").to_string();
    let text = json_str(&json, "text", "").to_string();
    let scope_s = json_str(&json, "scope", "broadcast");
    let sender_kind_s = json_str(&json, "sender_kind", "agent");

    let scope = match scope_s {
        "mentions" => MessageScope::Mentions,
        _ => MessageScope::Broadcast,
    };
    let sender_kind = match sender_kind_s {
        "system" | "sys" => SenderKind::System,
        "user" | "human" | "external" => SenderKind::External,
        _ => SenderKind::Instance,
    };

    let recipients: Vec<String> = json
        .get("mentions")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let delivered: Vec<String> = json
        .get("delivered_to")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let intent = json
        .get("intent")
        .and_then(|v| v.as_str())
        .map(String::from);
    let reply_to = json.get("reply_to").and_then(|v| v.as_u64());

    Some(Message {
        event_id: id as u64,
        sender: from,
        recipients,
        body: text,
        time: parse_iso_to_epoch(timestamp),
        delivered,
        scope,
        sender_kind,
        intent,
        reply_to,
    })
}

fn parse_status_or_life_row(
    id: i64,
    timestamp: &str,
    instance: &str,
    event_type: &str,
    data: &str,
) -> Option<Event> {
    let json: serde_json::Value = serde_json::from_str(data).ok()?;

    if event_type == "life" {
        let action = json_str(&json, "action", "unknown");
        let kind = match action {
            "ready" | "started" => ActivityKind::Started,
            "stopped" | "killed" => ActivityKind::Stopped,
            _ => ActivityKind::StateChange,
        };
        let reason = json_str(&json, "reason", "");
        let by = json_str(&json, "by", "");
        let mut detail_text = action.to_string();
        let mut sub_lines = Vec::new();
        match action {
            "stopped" | "killed" => {
                if !reason.is_empty() || !by.is_empty() {
                    let mut parts = Vec::new();
                    if !by.is_empty() {
                        parts.push(format!("by {}", by));
                    }
                    if !reason.is_empty() {
                        parts.push(reason.to_string());
                    }
                    detail_text = format!("{} ({})", action, parts.join(", "));
                }
                if let Some(snap) = json.get("snapshot") {
                    let tool = json_str(snap, "tool", "");
                    let dir = json_str(snap, "directory", "");
                    let session = json_str(snap, "session_id", "");
                    let pid = snap.get("pid").and_then(|v| v.as_u64()).unwrap_or(0);
                    let created_at = snap
                        .get("created_at")
                        .and_then(|v| {
                            v.as_f64()
                                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                        })
                        .unwrap_or(0.0);
                    let event_time = parse_iso_to_epoch(timestamp);

                    let mut line1 = format!("tool: {}  pid: {}", tool, pid);
                    if created_at > 0.0 && event_time > created_at {
                        let dur = (event_time - created_at) as u64;
                        let dur_str = if dur < 60 {
                            format!("{}s", dur)
                        } else if dur < 3600 {
                            format!("{}m {}s", dur / 60, dur % 60)
                        } else {
                            format!("{}h {}m", dur / 3600, (dur % 3600) / 60)
                        };
                        line1.push_str(&format!("  ran: {}", dur_str));
                    }
                    sub_lines.push(line1);

                    if !dir.is_empty() {
                        sub_lines.push(format!("dir: {}", truncate_path(dir, 60)));
                    }
                    if !session.is_empty() {
                        sub_lines.push(format!("session: {}", session));
                    }
                    let mut extras = Vec::new();
                    let tag = json_str(snap, "tag", "");
                    if !tag.is_empty() {
                        extras.push(format!("tag: {}", tag));
                    }
                    let parent = json_str(snap, "parent_name", "");
                    if !parent.is_empty() {
                        extras.push(format!("parent: {}", parent));
                    }
                    if !extras.is_empty() {
                        sub_lines.push(extras.join(" | "));
                    }
                    sub_lines.push(format!("resume: hcom r {}", instance));
                }
            }
            "batch_launched" => {
                let tool_raw = json_str(&json, "tool", "");
                let tool = tool_raw.strip_suffix("-pty").unwrap_or(tool_raw);
                let count = json.get("launched").and_then(|v| v.as_u64()).unwrap_or(0);
                let tag = json_str(&json, "tag", "");
                let mut text = format!("launched {}× {}", count, tool);
                if !tag.is_empty() {
                    text.push_str(&format!(" #{}", tag));
                }
                if !by.is_empty() {
                    text.push_str(&format!(" (by {})", by));
                }
                detail_text = text;
            }
            "created" | "started" => {
                let mut parts = Vec::new();
                if !by.is_empty() {
                    parts.push(format!("by {}", by));
                }
                if !reason.is_empty() {
                    parts.push(reason.to_string());
                }
                if !parts.is_empty() {
                    detail_text = format!("{} ({})", action, parts.join(", "));
                }
            }
            "relay_device_join" | "relay_device_leave" => {
                // relay::ACTION_DEVICE_*
                let text = json_str(&json, "text", action);
                detail_text = text.to_string();
            }
            _ => {}
        }
        return Some(Event {
            row_id: id as u64,
            agent: instance.to_string(),
            time: parse_iso_to_epoch(timestamp),
            kind: EventKind::Activity(kind),
            tool: String::new(),
            detail: detail_text,
            sub_lines,
        });
    }

    // status events
    let status = json_str(&json, "status", "active");
    let context = json_str(&json, "context", "");
    let detail = json_str(&json, "detail", "");

    let (kind, tool_name, detail_text) = if status == ST_ACTIVE && context.starts_with("tool:") {
        let t = context.strip_prefix("tool:").unwrap_or(context);
        // Suppress hcom send commands — the resulting message is already shown
        if detail.starts_with("hcom send") {
            return None;
        }
        (EventKind::Tool, t.to_string(), detail.to_string())
    } else {
        let activity = match status {
            "stopped" | "inactive" => ActivityKind::Stopped,
            "blocked" => ActivityKind::Blocked,
            "listening" => ActivityKind::Listening,
            "active" => ActivityKind::Active,
            _ => ActivityKind::StateChange,
        };
        let text = match (context.is_empty(), detail.is_empty()) {
            (true, true) => status.to_string(),
            (false, true) => context.to_string(),
            (true, false) => detail.to_string(),
            (false, false) => format!("{}: {}", context, detail),
        };
        (EventKind::Activity(activity), String::new(), text)
    };

    Some(Event {
        row_id: id as u64,
        agent: instance.to_string(),
        time: parse_iso_to_epoch(timestamp),
        kind,
        tool: tool_name,
        detail: detail_text,
        sub_lines: vec![],
    })
}

/// Truncate a path to max_len characters, replacing the middle with "...".
fn truncate_path(path: &str, max_len: usize) -> String {
    let chars: Vec<char> = path.chars().collect();
    if chars.len() <= max_len {
        return path.to_string();
    }
    let keep = max_len.saturating_sub(3) / 2;
    let head: String = chars[..keep].iter().collect();
    let tail: String = chars[chars.len() - keep..].iter().collect();
    format!("{}...{}", head, tail)
}

// ── Relay ───────────────────────────────────────────────────────

/// Read (configured, enabled) flags from config.toml. `configured` is true
/// when `relay.id` is a non-empty string; `enabled` is the `relay.enabled`
/// flag (defaults true when absent — matches HcomConfig::default).
fn read_relay_config_flags() -> (bool, bool) {
    let Some(table) = read_config_toml() else {
        return (false, false);
    };
    let Some(relay) = table.get("relay").and_then(|v| v.as_table()) else {
        return (false, false);
    };
    let Some(id) = relay.get("id").and_then(|v| v.as_str()) else {
        return (false, false);
    };
    if id.is_empty() {
        return (false, false);
    }
    let enabled = relay
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    (true, enabled)
}

/// Read a non-empty string value from the kv table. Returns None if missing or empty.
fn kv_get(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM kv WHERE key = ?", params![key], |row| {
        row.get(0)
    })
    .ok()
    .filter(|s: &String| !s.is_empty())
}

/// Build a RelayObservation from the TUI's raw connection + pidfile + caller-
/// supplied config flags, then derive RelayHealth. The TUI snapshot path
/// doesn't carry an HcomConfig/HcomDb pair, so we reconstruct the observation
/// locally rather than threading those types through every snapshot call.
/// `configured` and `enabled` are passed in so the caller can read config.toml
/// once per snapshot rather than us re-reading it here.
fn load_relay_state(
    conn: &Connection,
    configured: bool,
    enabled: bool,
) -> crate::relay::RelayHealth {
    let raw_status = kv_get(conn, "relay_status");
    let raw_error = kv_get(conn, "relay_last_error");
    let heartbeat_age_s = kv_get(conn, crate::relay::HEARTBEAT_KEY)
        .and_then(|s| s.parse::<f64>().ok())
        .map(|ts| (crate::shared::time::now_epoch_f64() - ts).max(0.0));
    let pidfile = crate::relay::worker::observe_pid_file();

    let obs = crate::relay::RelayObservation {
        configured,
        enabled,
        raw_status,
        raw_error,
        heartbeat_age_s,
        pidfile,
        last_push: 0.0,
        broker: None,
    };
    crate::relay::derive_relay_health(&obs)
}

// ── Timestamp parsing ───────────────────────────────────────────

fn parse_iso_to_epoch(ts: &str) -> f64 {
    use chrono::{DateTime, NaiveDateTime};
    // DB timestamps are RFC 3339 with microseconds: "2026-02-17T08:32:37.525519+00:00"
    if let Ok(dt) = DateTime::parse_from_rfc3339(ts) {
        dt.timestamp() as f64 + (dt.timestamp_subsec_millis() as f64 / 1000.0)
    } else if let Ok(dt) = NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S") {
        dt.and_utc().timestamp() as f64
    } else {
        0.0
    }
}

// ── Launch defaults from config.toml ─────────────────────────────

pub struct LaunchDefaults {
    pub terminal: String,
    pub tag: String,
}

impl Default for LaunchDefaults {
    fn default() -> Self {
        Self {
            terminal: "default".into(),
            tag: String::new(),
        }
    }
}

fn config_toml_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(paths::config_toml_path())
        .and_then(|m| m.modified())
        .ok()
}

fn read_config_toml() -> Option<toml::Table> {
    let content = std::fs::read_to_string(paths::config_toml_path()).ok()?;
    content.parse().ok()
}

pub fn read_launch_defaults() -> LaunchDefaults {
    let table = match read_config_toml() {
        Some(t) => t,
        None => return LaunchDefaults::default(),
    };

    let terminal = table
        .get("terminal")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("active"))
        .and_then(|v| v.as_str())
        .unwrap_or("default")
        .to_string();

    let tag = table
        .get("launch")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("tag"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    LaunchDefaults { terminal, tag }
}

// ── Dynamic terminal preset detection ────────────────────────────

/// Built-in preset: name, binary to check (None = macOS app bundle), app name, platforms.
struct PresetDef {
    name: &'static str,
    binary: Option<&'static str>,
    app_name: &'static str,
    platforms: &'static [&'static str],
}

/// All built-in presets (canonical list in shared/constants.rs TERMINAL_PRESETS).
/// Order here determines display order in the TUI.
const BUILTIN_PRESETS: &[PresetDef] = &[
    // macOS native
    PresetDef {
        name: "terminal.app",
        binary: None,
        app_name: "Terminal",
        platforms: &["Darwin"],
    },
    PresetDef {
        name: "iterm",
        binary: None,
        app_name: "iTerm",
        platforms: &["Darwin"],
    },
    PresetDef {
        name: "ghostty",
        binary: None,
        app_name: "Ghostty",
        platforms: &["Darwin"],
    },
    // Cross-platform
    PresetDef {
        name: "kitty",
        binary: Some("kitty"),
        app_name: "kitty",
        platforms: &["Darwin", "Linux"],
    },
    PresetDef {
        name: "wezterm",
        binary: Some("wezterm"),
        app_name: "WezTerm",
        platforms: &["Darwin", "Linux", "Windows"],
    },
    PresetDef {
        name: "alacritty",
        binary: Some("alacritty"),
        app_name: "Alacritty",
        platforms: &["Darwin", "Linux", "Windows"],
    },
    // Multiplexer
    PresetDef {
        name: "tmux",
        binary: Some("tmux"),
        app_name: "",
        platforms: &["Darwin", "Linux"],
    },
    // Tab utilities
    PresetDef {
        name: "ttab",
        binary: Some("ttab"),
        app_name: "",
        platforms: &["Darwin"],
    },
    // Linux-only
    PresetDef {
        name: "gnome-terminal",
        binary: Some("gnome-terminal"),
        app_name: "",
        platforms: &["Linux"],
    },
    PresetDef {
        name: "konsole",
        binary: Some("konsole"),
        app_name: "",
        platforms: &["Linux"],
    },
    PresetDef {
        name: "xterm",
        binary: Some("xterm"),
        app_name: "",
        platforms: &["Linux"],
    },
    PresetDef {
        name: "tilix",
        binary: Some("tilix"),
        app_name: "",
        platforms: &["Linux"],
    },
    PresetDef {
        name: "terminator",
        binary: Some("terminator"),
        app_name: "",
        platforms: &["Linux"],
    },
    // Windows
    PresetDef {
        name: "windows-terminal",
        binary: Some("wt"),
        app_name: "",
        platforms: &["Windows"],
    },
];

fn current_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "Darwin"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else {
        "Unknown"
    }
}

/// Check if a binary is on PATH.
fn binary_available(name: &str) -> bool {
    crate::terminal::which_bin(name).is_some()
}

/// Check if a macOS .app bundle exists in standard locations.
fn macos_app_available(app_name: &str) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }
    let bundle = if app_name.ends_with(".app") {
        app_name.to_string()
    } else {
        format!("{}.app", app_name)
    };
    let static_dirs = [
        "/Applications",
        "/System/Applications",
        "/System/Applications/Utilities",
    ];
    if static_dirs
        .iter()
        .any(|d| std::path::Path::new(d).join(&bundle).exists())
    {
        return true;
    }
    std::env::var("HOME")
        .ok()
        .map(|home| {
            std::path::Path::new(&home)
                .join("Applications")
                .join(&bundle)
                .exists()
        })
        .unwrap_or(false)
}

/// Get terminal presets available on this system.
/// Always starts with "default", then lists detected presets.
/// Also includes user-defined presets from config.toml.
pub fn get_available_presets() -> Vec<String> {
    let platform = current_platform();
    let mut result = vec!["default".to_string()];

    for preset in BUILTIN_PRESETS {
        if !preset.platforms.contains(&platform) {
            continue;
        }
        let available = match preset.binary {
            Some(bin) => {
                binary_available(bin)
                    || (cfg!(target_os = "macos")
                        && !preset.app_name.is_empty()
                        && macos_app_available(preset.app_name))
            }
            None => {
                // macOS app bundle check only
                if cfg!(target_os = "macos") {
                    macos_app_available(preset.app_name)
                } else {
                    true
                }
            }
        };
        if available {
            result.push(preset.name.to_string());
        }
    }

    // User-defined presets from config.toml [terminal.presets.*]
    if let Some(table) = read_config_toml() {
        if let Some(presets_table) = table
            .get("terminal")
            .and_then(|v| v.as_table())
            .and_then(|t| t.get("presets"))
            .and_then(|v| v.as_table())
        {
            for name in presets_table.keys() {
                if !result.iter().any(|r| r == name) {
                    result.push(name.clone());
                }
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::{
        compute_unread_batch, count_gt, load_instances, load_recently_stopped, parse_message_row,
        parse_status_or_life_row,
    };
    use crate::tui::model::{
        ActivityKind, Agent, AgentStatus, EventKind, MessageScope, SenderKind, Tool,
    };
    use rusqlite::Connection;

    fn setup_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE events (
                id INTEGER PRIMARY KEY,
                type TEXT NOT NULL,
                instance TEXT NOT NULL,
                data TEXT NOT NULL,
                timestamp TEXT NOT NULL
            );
            CREATE TABLE instances (
                name TEXT PRIMARY KEY
            );
            ",
        )
        .unwrap();
        conn
    }

    fn make_agent(name: &str, last_event_id: u64) -> Agent {
        Agent {
            name: name.into(),
            tool: Tool::Claude,
            status: AgentStatus::Active,
            status_context: String::new(),
            status_detail: String::new(),
            created_at: 0.0,
            status_time: 0.0,
            last_heartbeat: 0.0,
            has_tcp: false,
            directory: String::new(),
            tag: String::new(),
            unread: 0,
            last_event_id: Some(last_event_id),
            device_name: None,
            sync_age: None,
            headless: false,
            session_id: None,
            pid: None,
            terminal_preset: None,
        }
    }

    #[test]
    fn load_recently_stopped_filters_older_than_ten_minutes() {
        let conn = setup_conn();
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-18T00:10:00+00:00")
            .unwrap()
            .timestamp() as f64;

        conn.execute(
            "INSERT INTO events (id, type, instance, data, timestamp) VALUES (?, ?, ?, ?, ?)",
            rusqlite::params![
                1i64,
                "life",
                "olda",
                r#"{"action":"stopped","snapshot":{"tool":"claude"}}"#,
                "2026-02-17T23:59:00+00:00"
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events (id, type, instance, data, timestamp) VALUES (?, ?, ?, ?, ?)",
            rusqlite::params![
                2i64,
                "life",
                "reca",
                r#"{"action":"killed","snapshot":{"tool":"gemini"}}"#,
                "2026-02-18T00:09:30+00:00"
            ],
        )
        .unwrap();

        let stopped = load_recently_stopped(&conn, now);
        assert_eq!(stopped.len(), 1);
        assert_eq!(stopped[0].name, "reca");
    }

    #[test]
    fn load_recently_stopped_excludes_currently_active_instances() {
        let conn = setup_conn();
        let now = chrono::DateTime::parse_from_rfc3339("2026-02-18T00:10:00+00:00")
            .unwrap()
            .timestamp() as f64;

        conn.execute(
            "INSERT INTO instances (name) VALUES (?)",
            rusqlite::params!["live"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events (id, type, instance, data, timestamp) VALUES (?, ?, ?, ?, ?)",
            rusqlite::params![
                1i64,
                "life",
                "live",
                r#"{"action":"stopped","snapshot":{"tool":"claude"}}"#,
                "2026-02-18T00:09:30+00:00"
            ],
        )
        .unwrap();

        let stopped = load_recently_stopped(&conn, now);
        assert!(stopped.is_empty());
    }

    #[test]
    fn unread_batch_counts_mentions_and_broadcasts_without_self_messages() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE events (
                id INTEGER PRIMARY KEY,
                type TEXT NOT NULL,
                data TEXT NOT NULL
            );
            ",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO events (id, type, data) VALUES (1, 'message', ?)",
            rusqlite::params![r#"{"from":"aone","scope":"broadcast","sender_kind":"agent"}"#],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events (id, type, data) VALUES (2, 'message', ?)",
            rusqlite::params![r#"{"from":"sys","scope":"broadcast","sender_kind":"system"}"#],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events (id, type, data) VALUES (3, 'message', ?)",
            rusqlite::params![
                r#"{"from":"cone","scope":"mentions","mentions":["aone","btwo"],"sender_kind":"agent"}"#
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events (id, type, data) VALUES (4, 'message', ?)",
            rusqlite::params![r#"{"from":"btwo","scope":"broadcast","sender_kind":"agent"}"#],
        )
        .unwrap();

        let mut agents = vec![make_agent("aone", 0), make_agent("btwo", 2)];
        compute_unread_batch(&conn, &mut agents);

        assert_eq!(agents[0].unread, 2, "aone: mention(3) + broadcast(4)");
        assert_eq!(
            agents[1].unread, 1,
            "btwo: mention(3), self broadcast(4) ignored"
        );
    }

    #[test]
    fn unknown_status_defaults_to_inactive_in_instance_load() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE instances (
                name TEXT,
                tool TEXT,
                status TEXT,
                status_context TEXT,
                status_detail TEXT,
                created_at REAL,
                status_time INTEGER,
                last_stop INTEGER,
                tcp_mode INTEGER,
                directory TEXT,
                tag TEXT,
                last_event_id INTEGER,
                origin_device_id TEXT,
                pid INTEGER,
                session_id TEXT,
                background INTEGER,
                terminal_preset_effective TEXT
            );
            CREATE TABLE kv (key TEXT, value TEXT);
            ",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO instances (name, tool, status, status_context, status_detail, created_at, status_time, last_stop, tcp_mode, directory, tag, last_event_id, origin_device_id, pid, session_id, background, terminal_preset_effective)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                "nazo",
                "claude",
                "teleporting",
                "",
                "",
                10.0f64,
                0i64,
                0i64,
                0i64,
                "/tmp",
                "",
                0i64,
                "",
                Option::<i64>::None,
                Option::<String>::None,
                0i64,
                Option::<String>::None
            ],
        )
        .unwrap();

        let (local, remote) = load_instances(&conn, "", 100.0);
        assert!(remote.is_empty());
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].status, AgentStatus::Inactive);
        assert_eq!(local[0].status_context, "unknown_status");
        assert!(local[0].status_detail.contains("teleporting"));
    }

    #[test]
    fn parse_message_row_defaults_missing_fields() {
        let msg = parse_message_row(7, "2026-02-18T00:09:30+00:00", "{}").unwrap();
        assert_eq!(msg.event_id, 7);
        assert_eq!(msg.sender, "?");
        assert_eq!(msg.body, "");
        assert_eq!(msg.scope, MessageScope::Broadcast);
        assert_eq!(msg.sender_kind, SenderKind::Instance);
        assert!(msg.recipients.is_empty());
        assert!(msg.delivered.is_empty());
        assert!(msg.intent.is_none());
        assert!(msg.reply_to.is_none());
    }

    #[test]
    fn parse_message_row_maps_mentions_and_external_sender_kind() {
        let msg = parse_message_row(
            8,
            "2026-02-18T00:09:30+00:00",
            r#"{"from":"sys","text":"hi","scope":"mentions","sender_kind":"human","mentions":["nova"],"delivered_to":["nova"],"intent":"request","reply_to":42}"#,
        )
        .unwrap();

        assert_eq!(msg.scope, MessageScope::Mentions);
        assert_eq!(msg.sender_kind, SenderKind::External);
        assert_eq!(msg.recipients, vec!["nova"]);
        assert_eq!(msg.delivered, vec!["nova"]);
        assert_eq!(msg.intent.as_deref(), Some("request"));
        assert_eq!(msg.reply_to, Some(42));
    }

    #[test]
    fn parse_status_row_with_tool_context_creates_tool_event() {
        let ev = parse_status_or_life_row(
            9,
            "2026-02-18T00:09:30+00:00",
            "nova",
            "status",
            r#"{"status":"active","context":"tool:Read","detail":"src/lib.rs"}"#,
        )
        .unwrap();

        assert_eq!(ev.row_id, 9);
        assert_eq!(ev.agent, "nova");
        assert_eq!(ev.kind, EventKind::Tool);
        assert_eq!(ev.tool, "Read");
        assert_eq!(ev.detail, "src/lib.rs");
    }

    #[test]
    fn parse_life_stopped_event_includes_resume_subline() {
        let ev = parse_status_or_life_row(
            10,
            "2026-02-18T00:10:00+00:00",
            "nova",
            "life",
            r#"{"action":"stopped","reason":"idle","by":"bigboss","snapshot":{"tool":"claude","pid":1234,"directory":"/tmp/demo","created_at":1739837340.0}}"#,
        )
        .unwrap();

        assert_eq!(ev.kind, EventKind::Activity(ActivityKind::Stopped));
        assert!(ev.detail.contains("stopped"));
        assert!(ev.detail.contains("bigboss"));
        assert!(
            ev.sub_lines
                .iter()
                .any(|l| l.starts_with("resume: hcom r nova")),
            "expected resume hint in sub-lines, got {:?}",
            ev.sub_lines
        );
    }

    #[test]
    fn count_gt_works_with_sorted_ids() {
        let ids = vec![2, 4, 9, 12];
        assert_eq!(count_gt(&ids, 0), 4);
        assert_eq!(count_gt(&ids, 4), 2);
        assert_eq!(count_gt(&ids, 12), 0);
    }
}
