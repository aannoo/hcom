//! Track hcom-launched process PIDs for orphan detection and recovery.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const PIDFILE_NAME: &str = ".tmp/launched_pids.json";

/// Tracked process entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PidEntry {
    pub tool: String,
    pub names: Vec<String>,
    pub launched_at: f64,
    #[serde(default)]
    pub directory: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub process_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub terminal_preset: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub terminal_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kitty_listen_on: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session_id: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub notify_port: u16,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub inject_port: u16,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tag: String,
}

fn is_zero(v: &u16) -> bool {
    *v == 0
}

/// Orphan process info (enriched with PID).
#[derive(Debug, Clone)]
pub struct OrphanProcess {
    pub pid: u32,
    pub tool: String,
    pub names: Vec<String>,
    pub directory: String,
    pub process_id: String,
    pub terminal_preset: String,
    pub pane_id: String,
    pub terminal_id: String,
    pub kitty_listen_on: String,
    pub session_id: String,
    pub notify_port: u16,
    pub inject_port: u16,
    pub tag: String,
}

impl From<(u32, &PidEntry)> for OrphanProcess {
    fn from((pid, entry): (u32, &PidEntry)) -> Self {
        Self {
            pid,
            tool: entry.tool.clone(),
            names: entry.names.clone(),
            directory: entry.directory.clone(),
            process_id: entry.process_id.clone(),
            terminal_preset: entry.terminal_preset.clone(),
            pane_id: entry.pane_id.clone(),
            terminal_id: entry.terminal_id.clone(),
            kitty_listen_on: entry.kitty_listen_on.clone(),
            session_id: entry.session_id.clone(),
            notify_port: entry.notify_port,
            inject_port: entry.inject_port,
            tag: entry.tag.clone(),
        }
    }
}

/// Resolve the pidfile path from hcom_dir.
fn pidfile_path(hcom_dir: &Path) -> PathBuf {
    hcom_dir.join(PIDFILE_NAME)
}

/// Check if a process is alive via `kill(pid, 0)`.
/// Handles EPERM (process exists but owned by another user).
pub fn is_alive(pid: u32) -> bool {
    // SAFETY: kill(pid, 0) is a no-op signal that just checks process existence.
    let ret = unsafe { libc::kill(pid as i32, 0) };
    if ret == 0 {
        return true;
    }
    // EPERM means process exists but is owned by another user
    let err = std::io::Error::last_os_error();
    err.raw_os_error() == Some(libc::EPERM)
}

/// Read raw pidfile data.
fn read_raw(hcom_dir: &Path) -> HashMap<String, PidEntry> {
    match std::fs::read_to_string(pidfile_path(hcom_dir)) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => HashMap::new(),
    }
}

/// Write pidfile data atomically (temp + rename).
fn write_raw(hcom_dir: &Path, data: &HashMap<String, PidEntry>) {
    if let Ok(content) = serde_json::to_string(data) {
        crate::paths::atomic_write(&pidfile_path(hcom_dir), &content);
    }
}

/// Parameters for recording a launched process.
#[derive(Debug)]
pub struct PidRecord<'a> {
    pub hcom_dir: &'a Path,
    pub pid: u32,
    pub tool: &'a str,
    pub name: &'a str,
    pub directory: &'a str,
    pub process_id: &'a str,
    pub terminal_preset: &'a str,
    pub pane_id: &'a str,
    pub terminal_id: &'a str,
    pub kitty_listen_on: &'a str,
    pub session_id: &'a str,
    pub notify_port: u16,
    pub inject_port: u16,
    pub tag: &'a str,
}

impl<'a> PidRecord<'a> {
    /// Create with required fields, defaulting optional ones.
    pub fn new(
        hcom_dir: &'a Path,
        pid: u32,
        tool: &'a str,
        name: &'a str,
        directory: &'a str,
    ) -> Self {
        Self {
            hcom_dir,
            pid,
            tool,
            name,
            directory,
            process_id: "",
            terminal_preset: "",
            pane_id: "",
            terminal_id: "",
            kitty_listen_on: "",
            session_id: "",
            notify_port: 0,
            inject_port: 0,
            tag: "",
        }
    }
}

/// Record a launched process PID.
pub fn record_pid(rec: &PidRecord<'_>) {
    let PidRecord {
        hcom_dir,
        pid,
        tool,
        name,
        directory,
        process_id,
        terminal_preset,
        pane_id,
        terminal_id,
        kitty_listen_on,
        session_id,
        notify_port,
        inject_port,
        tag,
    } = rec;
    let mut data = read_raw(hcom_dir);
    let key = pid.to_string();

    if let Some(entry) = data.get_mut(&key) {
        // Append name if not already present
        if !entry.names.contains(&name.to_string()) {
            entry.names.push(name.to_string());
        }
        // Fill in fields that are empty
        if !process_id.is_empty() && entry.process_id.is_empty() {
            entry.process_id = process_id.to_string();
        }
        if !terminal_preset.is_empty() && entry.terminal_preset.is_empty() {
            entry.terminal_preset = terminal_preset.to_string();
        }
        if !pane_id.is_empty() && entry.pane_id.is_empty() {
            entry.pane_id = pane_id.to_string();
        }
        if !terminal_id.is_empty() && entry.terminal_id.is_empty() {
            entry.terminal_id = terminal_id.to_string();
        }
        if !kitty_listen_on.is_empty() && entry.kitty_listen_on.is_empty() {
            entry.kitty_listen_on = kitty_listen_on.to_string();
        }
        if !session_id.is_empty() && entry.session_id.is_empty() {
            entry.session_id = session_id.to_string();
        }
        if *notify_port != 0 && entry.notify_port == 0 {
            entry.notify_port = *notify_port;
        }
        if *inject_port != 0 && entry.inject_port == 0 {
            entry.inject_port = *inject_port;
        }
        if !tag.is_empty() && entry.tag.is_empty() {
            entry.tag = tag.to_string();
        }
    } else {
        data.insert(
            key,
            PidEntry {
                tool: tool.to_string(),
                names: vec![name.to_string()],
                launched_at: crate::shared::time::now_epoch_f64(),
                directory: directory.to_string(),
                process_id: process_id.to_string(),
                terminal_preset: terminal_preset.to_string(),
                pane_id: pane_id.to_string(),
                terminal_id: terminal_id.to_string(),
                kitty_listen_on: kitty_listen_on.to_string(),
                session_id: session_id.to_string(),
                notify_port: *notify_port,
                inject_port: *inject_port,
                tag: tag.to_string(),
            },
        );
    }

    write_raw(hcom_dir, &data);
}

/// Get running hcom processes not accounted for by active instances.
///
/// Auto-prunes dead PIDs from the file. If `active_pids` is provided,
/// also prunes PIDs that are now active from the file and filters them
/// from the result.
pub fn get_orphan_processes(
    hcom_dir: &Path,
    active_pids: Option<&std::collections::HashSet<u32>>,
) -> Vec<OrphanProcess> {
    let data = read_raw(hcom_dir);

    // Filter to alive processes only
    let mut alive: HashMap<String, PidEntry> = HashMap::new();
    for (pid_str, entry) in &data {
        if let Ok(pid) = pid_str.parse::<u32>() {
            if is_alive(pid) {
                alive.insert(pid_str.clone(), entry.clone());
            }
        }
    }

    // Write back pruned data if anything was removed
    if alive.len() != data.len() {
        write_raw(hcom_dir, &alive);
    }

    // Build result
    let mut result: Vec<OrphanProcess> = alive
        .iter()
        .filter_map(|(pid_str, entry)| {
            pid_str
                .parse::<u32>()
                .ok()
                .map(|pid| OrphanProcess::from((pid, entry)))
        })
        .collect();

    // Prune active PIDs from file and filter from result
    if let Some(active) = active_pids {
        let active_in_file: Vec<String> = result
            .iter()
            .filter(|p| active.contains(&p.pid))
            .map(|p| p.pid.to_string())
            .collect();
        if !active_in_file.is_empty() {
            let mut pruned = alive;
            for k in &active_in_file {
                pruned.remove(k);
            }
            write_raw(hcom_dir, &pruned);
        }
        result.retain(|p| !active.contains(&p.pid));
    }

    result
}

/// Remove a PID from tracking (after kill).
pub fn remove_pid(hcom_dir: &Path, pid: u32) {
    let mut data = read_raw(hcom_dir);
    let key = pid.to_string();
    if data.remove(&key).is_some() {
        write_raw(hcom_dir, &data);
    }
}

/// Re-register a single orphan into the DB.
///
/// Creates instance row, sets PID/directory, creates process/session bindings,
/// and sets status to listening so the PTY delivery gate can inject messages.
/// Does NOT log events, print output, or remove from pidtrack — caller handles those.
///
/// Returns `Err` if the critical instance INSERT fails (caller must not prune
/// the pidtrack entry on failure —
pub fn recover_single_orphan_to_db(
    db: &crate::db::HcomDb,
    orphan: &OrphanProcess,
    instance_name: &str,
) -> Result<(), String> {
    use crate::instances;
    use crate::shared::constants::ST_LISTENING;

    let now = crate::shared::time::now_epoch_i64();

    // Create instance row — this is the critical step; fail = abort recovery
    db.conn()
        .execute(
            "INSERT OR IGNORE INTO instances (name, tool, status, status_context, created_at) VALUES (?1, ?2, 'inactive', 'new', ?3)",
            rusqlite::params![instance_name, orphan.tool, now],
        )
        .map_err(|e| format!("failed to insert instance '{}': {}", instance_name, e))?;

    // Update PID and directory
    let mut updates = serde_json::Map::new();
    updates.insert("pid".into(), serde_json::json!(orphan.pid));
    if !orphan.directory.is_empty() {
        updates.insert("directory".into(), serde_json::json!(orphan.directory));
    }
    if !orphan.terminal_preset.is_empty() {
        updates.insert(
            "terminal_preset_effective".into(),
            serde_json::json!(orphan.terminal_preset),
        );
    }
    let mut launch_context = serde_json::Map::new();
    if !orphan.process_id.is_empty() {
        launch_context.insert("process_id".into(), serde_json::json!(orphan.process_id));
    }
    if !orphan.pane_id.is_empty() {
        launch_context.insert("pane_id".into(), serde_json::json!(orphan.pane_id));
    }
    if !orphan.terminal_id.is_empty() {
        launch_context.insert("terminal_id".into(), serde_json::json!(orphan.terminal_id));
    }
    if !orphan.kitty_listen_on.is_empty() {
        launch_context.insert(
            "kitty_listen_on".into(),
            serde_json::json!(orphan.kitty_listen_on),
        );
    }
    if !launch_context.is_empty() {
        updates.insert(
            "launch_context".into(),
            serde_json::json!(serde_json::to_string(&launch_context).unwrap_or_else(|_| "{}".to_string())),
        );
    }
    instances::update_instance_position(db, instance_name, &updates);

    // Create process binding
    if !orphan.process_id.is_empty() {
        let sid = if orphan.session_id.is_empty() {
            None
        } else {
            Some(orphan.session_id.as_str())
        };
        db.set_process_binding(&orphan.process_id, sid.unwrap_or(""), instance_name)
            .map_err(|e| format!("failed to set process binding: {}", e))?;
    }

    // Create session binding
    if !orphan.session_id.is_empty() {
        db.rebind_session(&orphan.session_id, instance_name)
            .map_err(|e| format!("failed to rebind session: {}", e))?;
        let mut sid_update = serde_json::Map::new();
        sid_update.insert("session_id".into(), serde_json::json!(orphan.session_id));
        instances::update_instance_position(db, instance_name, &sid_update);
    }

    // Restore notify endpoints
    if orphan.notify_port != 0 {
        db.register_notify_port(instance_name, orphan.notify_port)
            .map_err(|e| format!("failed to register notify port: {}", e))?;
    }
    if orphan.inject_port != 0 {
        db.register_inject_port(instance_name, orphan.inject_port)
            .map_err(|e| format!("failed to register inject port: {}", e))?;
    }

    // Set listening so PTY delivery gate allows message injection
    instances::set_status(
        db,
        instance_name,
        ST_LISTENING,
        "recovered",
        Default::default(),
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn make_temp_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".tmp")).unwrap();
        dir
    }

    fn rec<'a>(dir: &'a Path, pid: u32, tool: &'a str, name: &'a str) -> PidRecord<'a> {
        PidRecord::new(dir, pid, tool, name, "/tmp")
    }

    #[test]
    fn test_record_and_read() {
        let dir = make_temp_dir();
        record_pid(&PidRecord {
            hcom_dir: dir.path(),
            pid: 12345,
            tool: "claude",
            name: "luna",
            directory: "/tmp",
            process_id: "pid-1",
            terminal_preset: "kitty",
            pane_id: "pane-1",
            terminal_id: "term-1",
            kitty_listen_on: "/tmp/kitty.sock",
            session_id: "sess-1",
            notify_port: 8080,
            inject_port: 8081,
            tag: "test-tag",
        });

        let data = read_raw(dir.path());
        assert_eq!(data.len(), 1);
        let entry = data.get("12345").unwrap();
        assert_eq!(entry.tool, "claude");
        assert_eq!(entry.names, vec!["luna"]);
        assert_eq!(entry.process_id, "pid-1");
        assert_eq!(entry.terminal_preset, "kitty");
        assert_eq!(entry.pane_id, "pane-1");
        assert_eq!(entry.terminal_id, "term-1");
        assert_eq!(entry.kitty_listen_on, "/tmp/kitty.sock");
        assert_eq!(entry.session_id, "sess-1");
        assert_eq!(entry.notify_port, 8080);
        assert_eq!(entry.inject_port, 8081);
    }

    #[test]
    fn test_record_appends_name() {
        let dir = make_temp_dir();
        record_pid(&rec(dir.path(), 12345, "claude", "luna"));
        record_pid(&rec(dir.path(), 12345, "claude", "nova"));

        let data = read_raw(dir.path());
        let entry = data.get("12345").unwrap();
        assert_eq!(entry.names, vec!["luna", "nova"]);
    }

    #[test]
    fn test_record_fills_empty_fields() {
        let dir = make_temp_dir();
        record_pid(&rec(dir.path(), 12345, "claude", "luna"));
        record_pid(&PidRecord {
            process_id: "pid-1",
            terminal_preset: "kitty",
            notify_port: 8080,
            ..rec(dir.path(), 12345, "claude", "luna")
        });

        let data = read_raw(dir.path());
        let entry = data.get("12345").unwrap();
        assert_eq!(entry.process_id, "pid-1");
        assert_eq!(entry.terminal_preset, "kitty");
        assert_eq!(entry.notify_port, 8080);
    }

    #[test]
    fn test_record_does_not_overwrite_existing_fields() {
        let dir = make_temp_dir();
        record_pid(&PidRecord {
            process_id: "pid-1",
            terminal_preset: "kitty",
            ..rec(dir.path(), 12345, "claude", "luna")
        });
        record_pid(&PidRecord {
            process_id: "pid-2",
            terminal_preset: "wezterm",
            ..rec(dir.path(), 12345, "claude", "luna")
        });

        let data = read_raw(dir.path());
        let entry = data.get("12345").unwrap();
        assert_eq!(entry.process_id, "pid-1");
        assert_eq!(entry.terminal_preset, "kitty");
    }

    #[test]
    fn test_remove_pid() {
        let dir = make_temp_dir();
        record_pid(&rec(dir.path(), 12345, "claude", "luna"));
        record_pid(&rec(dir.path(), 67890, "gemini", "nova"));

        remove_pid(dir.path(), 12345);
        let data = read_raw(dir.path());
        assert_eq!(data.len(), 1);
        assert!(data.contains_key("67890"));
        assert!(!data.contains_key("12345"));
    }

    #[test]
    fn test_remove_nonexistent_pid() {
        let dir = make_temp_dir();
        record_pid(&rec(dir.path(), 12345, "claude", "luna"));
        remove_pid(dir.path(), 99999);
        let data = read_raw(dir.path());
        assert_eq!(data.len(), 1);
    }

    #[test]
    fn test_orphan_prunes_dead_pids() {
        let dir = make_temp_dir();
        record_pid(&rec(dir.path(), 99999999, "claude", "dead"));

        let orphans = get_orphan_processes(dir.path(), None);
        let data = read_raw(dir.path());
        assert!(!data.contains_key("99999999"));
        assert!(orphans.iter().all(|o| o.pid != 99999999));
    }

    #[test]
    fn test_orphan_active_pids_pruned() {
        let dir = make_temp_dir();
        let our_pid = std::process::id();
        record_pid(&rec(dir.path(), our_pid, "claude", "luna"));

        let mut active = HashSet::new();
        active.insert(our_pid);

        let orphans = get_orphan_processes(dir.path(), Some(&active));
        // Our PID is active — should be filtered from results AND pruned from file
        assert!(orphans.is_empty());
        let data = read_raw(dir.path());
        assert!(!data.contains_key(&our_pid.to_string()));
    }

    #[test]
    fn test_empty_pidfile() {
        let dir = make_temp_dir();
        let orphans = get_orphan_processes(dir.path(), None);
        assert!(orphans.is_empty());
    }

    #[test]
    fn test_is_alive_current_process() {
        assert!(is_alive(std::process::id()));
    }

    #[test]
    fn test_is_alive_dead_process() {
        assert!(!is_alive(99999999));
    }

    #[test]
    fn test_recover_single_orphan_returns_error_on_db_failure() {
        // DB without init_db → no instances table → INSERT fails
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = crate::db::HcomDb::open_raw(&db_path).unwrap();
        // Deliberately NOT calling db.init_db()

        let orphan = OrphanProcess {
            pid: std::process::id(),
            tool: "claude".into(),
            names: vec!["luna".into()],
            directory: "/tmp".into(),
            process_id: "pid-1".into(),
            terminal_preset: String::new(),
            pane_id: String::new(),
            terminal_id: String::new(),
            kitty_listen_on: String::new(),
            session_id: String::new(),
            notify_port: 0,
            inject_port: 0,
            tag: String::new(),
        };

        let result = recover_single_orphan_to_db(&db, &orphan, "luna");
        assert!(
            result.is_err(),
            "expected error when DB has no instances table"
        );
    }
}
