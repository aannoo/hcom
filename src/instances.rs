//! Instance classification predicates and generic row-update utilities.
//!
//! Display-name and identity resolution live in `identity.rs`; session/process
//! binding lives in `instance_binding.rs`. This module stays focused on small
//! helpers shared by commands, UI rendering, and lifecycle code.

use crate::db::{HcomDb, InstanceRow};
use crate::shared::ST_INACTIVE;

pub fn is_remote_instance(data: &InstanceRow) -> bool {
    data.origin_device_id.is_some()
}

pub fn is_subagent_instance(data: &InstanceRow) -> bool {
    data.parent_session_id.is_some()
}

/// Check if a session/instance is in subagent context (Task active).
///
/// Accepts either a session_id or instance name. Returns true if the instance
/// has active running tasks (i.e. a Task subagent is executing).
pub fn in_subagent_context(db: &HcomDb, session_id: &str) -> bool {
    let instance_name = match db.get_session_binding(session_id) {
        Ok(Some(name)) => name,
        _ => session_id.to_string(),
    };

    let row: Option<Option<String>> = db
        .conn()
        .query_row(
            "SELECT running_tasks FROM instances WHERE name = ? LIMIT 1",
            rusqlite::params![instance_name],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok();

    match row {
        Some(Some(rt_json)) if !rt_json.is_empty() => {
            let rt = parse_running_tasks(Some(&rt_json));
            rt.active
        }
        _ => false,
    }
}

pub fn is_launching_placeholder(data: &InstanceRow) -> bool {
    data.session_id.is_none()
        && data.status_context == "new"
        && (data.status == ST_INACTIVE || data.status == "pending")
}

/// Parsed running_tasks JSON field.
#[derive(Debug, Clone, Default)]
pub struct RunningTasks {
    pub active: bool,
    pub subagents: Vec<serde_json::Value>,
}

pub fn parse_running_tasks(json_str: Option<&str>) -> RunningTasks {
    let Some(s) = json_str else {
        return RunningTasks::default();
    };
    if s.is_empty() {
        return RunningTasks::default();
    }

    match serde_json::from_str::<serde_json::Value>(s) {
        Ok(serde_json::Value::Object(obj)) => RunningTasks {
            active: obj.get("active").and_then(|v| v.as_bool()).unwrap_or(false),
            subagents: obj
                .get("subagents")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default(),
        },
        _ => RunningTasks::default(),
    }
}

/// Update instance position atomically.
/// If instance doesn't exist, UPDATE silently affects 0 rows.
pub fn update_instance_position(
    db: &HcomDb,
    name: &str,
    updates: &serde_json::Map<String, serde_json::Value>,
) {
    let mut update_copy = updates.clone();
    for bool_field in &["tcp_mode", "background", "name_announced"] {
        if let Some(val) = update_copy.get(*bool_field)
            && let Some(b) = val.as_bool()
        {
            update_copy.insert(
                (*bool_field).to_string(),
                serde_json::json!(if b { 1 } else { 0 }),
            );
        }
    }

    if let Err(e) = db.update_instance_fields(name, &update_copy) {
        crate::log::log_error(
            "core",
            "db.error",
            &format!("update_instance_position: {} - {}", name, e),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instance_names::{
        CVCV_SPACE, allocate_name, banned_names, collect_taken_names, gold_names, hash_to_name,
        is_too_similar, name_pool, score_name,
    };
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn setup_test_db() -> (HcomDb, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let temp_dir = std::env::temp_dir();
        let test_id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let db_path = temp_dir.join(format!(
            "test_instances_{}_{}.db",
            std::process::id(),
            test_id
        ));

        let db = HcomDb::open_at(&db_path).unwrap();
        (db, db_path)
    }

    fn cleanup(path: PathBuf) {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
    }

    #[test]
    fn test_name_pool_populated() {
        let pool = name_pool();
        assert!(pool.len() > 1000, "pool should have >1000 names");
        let top_100: HashSet<&str> = pool[..100].iter().map(|x| x.name.as_str()).collect();
        assert!(top_100.contains("luna"), "luna should be in top 100");
        assert!(top_100.contains("nova"), "nova should be in top 100");
    }

    #[test]
    fn test_banned_names_excluded() {
        let pool = name_pool();
        let all_names: HashSet<&str> = pool.iter().map(|x| x.name.as_str()).collect();
        assert!(!all_names.contains("help"));
        assert!(!all_names.contains("send"));
        assert!(!all_names.contains("list"));
        assert!(!all_names.contains("stop"));
    }

    #[test]
    fn test_gold_names_score_higher() {
        let gold = gold_names();
        let banned = banned_names();
        let gold_score = score_name("luna", &gold, &banned);
        let non_gold = score_name("bxzx", &gold, &banned);
        assert!(gold_score > non_gold, "gold names should score higher");
    }

    #[test]
    fn test_hamming_similarity_check() {
        let mut alive = HashSet::new();
        alive.insert("luna".to_string());

        assert!(is_too_similar("lina", &alive));
        assert!(is_too_similar("luno", &alive));
        assert!(is_too_similar("lino", &alive));
        assert!(!is_too_similar("miso", &alive));
        assert!(!is_too_similar("kira", &alive));
    }

    #[test]
    fn test_allocate_name_avoids_taken() {
        let taken: HashSet<String> = ["luna", "nova", "kira"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let alive = taken.clone();
        let name = allocate_name(&|n| taken.contains(n), &alive, 200, 1200, 30.0).unwrap();
        assert!(!taken.contains(&name));
    }

    #[test]
    fn test_allocate_name_avoids_alive_first_letter() {
        // Forces the deterministic greedy tier (attempts=0 skips weighted
        // sampling) so the spread penalty's effect on adjusted ordering can
        // be asserted without RNG.
        let alive: HashSet<String> = [
            "luna", "lola", "lara", "lana", "lena", "lina", "lori", "loki",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let name = allocate_name(&|n| alive.contains(n), &alive, 0, 1200, 30.0).unwrap();
        assert!(
            !name.starts_with('l'),
            "greedy pick under spread penalty should avoid `l`, got {name}"
        );
    }

    #[test]
    fn test_hash_to_name_deterministic() {
        let n1 = hash_to_name("device-123", 0);
        let n2 = hash_to_name("device-123", 0);
        assert_eq!(n1, n2);
    }

    #[test]
    fn test_hash_to_name_collision_avoidance() {
        let n1 = hash_to_name("device-123", 0);
        let n2 = hash_to_name("device-123", 1);
        assert_ne!(n1, n2);
    }

    #[test]
    fn test_hash_to_name_probing_covers_full_cvcv_space() {
        // Walking attempts 0..CVCV_SPACE must visit every distinct CVCV
        // output exactly once — relay collision probing relies on this so
        // fallback only triggers when every slot is genuinely taken.
        let outputs: HashSet<String> = (0..CVCV_SPACE)
            .map(|a| hash_to_name("device-123", a as u32))
            .collect();
        assert_eq!(outputs.len(), CVCV_SPACE);
    }

    #[test]
    fn test_is_launching_placeholder() {
        let ph = InstanceRow {
            status: ST_INACTIVE.into(),
            status_context: "new".into(),
            session_id: None,
            ..default_instance()
        };
        assert!(is_launching_placeholder(&ph));

        let bound = InstanceRow {
            status: ST_INACTIVE.into(),
            status_context: "new".into(),
            session_id: Some("sid-123".into()),
            ..default_instance()
        };
        assert!(!is_launching_placeholder(&bound));
    }

    #[test]
    fn test_is_remote_instance() {
        let local = default_instance();
        assert!(!is_remote_instance(&local));

        let remote = InstanceRow {
            origin_device_id: Some("device-123".into()),
            ..default_instance()
        };
        assert!(is_remote_instance(&remote));
    }

    #[test]
    fn test_collect_taken_names_includes_stopped_snapshots() {
        let (db, path) = setup_test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, tool, status, status_context, created_at)
                 VALUES ('luna', 'codex', 'listening', 'start', 0)",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO events (timestamp, type, instance, data)
                 VALUES (strftime('%Y-%m-%dT%H:%M:%fZ','now'), 'life', 'vera', ?1)",
                rusqlite::params![serde_json::json!({"action": "stopped"}).to_string()],
            )
            .unwrap();

        let (alive_names, taken_names) = collect_taken_names(&db).unwrap();
        assert!(alive_names.contains("luna"));
        assert!(!alive_names.contains("vera"));
        assert!(taken_names.contains("luna"));
        assert!(taken_names.contains("vera"));

        cleanup(path);
    }

    #[test]
    fn test_collect_taken_names_ignores_placeholder_stops() {
        let (db, path) = setup_test_db();
        for (name, placeholder) in [("vera", true), ("zara", false)] {
            db.conn()
                .execute(
                    "INSERT INTO events (timestamp, type, instance, data)
                     VALUES (strftime('%Y-%m-%dT%H:%M:%fZ','now'), 'life', ?1, ?2)",
                    rusqlite::params![
                        name,
                        serde_json::json!({
                            "action": "stopped",
                            "placeholder": placeholder
                        })
                        .to_string()
                    ],
                )
                .unwrap();
        }

        let (_, taken_names) = collect_taken_names(&db).unwrap();
        assert!(!taken_names.contains("vera"));
        assert!(taken_names.contains("zara"));

        cleanup(path);
    }

    #[test]
    fn test_save_instance_reservation_does_not_replace_existing_row() {
        let (db, path) = setup_test_db();
        let mut data = serde_json::Map::new();
        data.insert("status".into(), serde_json::json!("pending"));
        data.insert("status_context".into(), serde_json::json!("new"));
        data.insert("created_at".into(), serde_json::json!(0.0));

        db.save_instance_reservation("luna", &data).unwrap();

        let mut replacement = serde_json::Map::new();
        replacement.insert("status".into(), serde_json::json!("listening"));
        assert!(db.save_instance_reservation("luna", &replacement).is_err());
        assert_eq!(
            db.get_instance_full("luna")
                .unwrap()
                .unwrap()
                .status
                .as_str(),
            "pending"
        );

        cleanup(path);
    }

    #[test]
    fn test_parse_running_tasks() {
        assert!(!parse_running_tasks(None).active);
        assert!(!parse_running_tasks(Some("")).active);
        assert!(!parse_running_tasks(Some("invalid")).active);

        let rt = parse_running_tasks(Some(r#"{"active":true,"subagents":[{"agent_id":"a1"}]}"#));
        assert!(rt.active);
        assert_eq!(rt.subagents.len(), 1);
    }

    fn default_instance() -> InstanceRow {
        InstanceRow {
            name: String::new(),
            session_id: None,
            parent_session_id: None,
            parent_name: None,
            agent_id: None,
            tag: None,
            last_event_id: 0,
            last_stop: 0,
            status: ST_INACTIVE.into(),
            status_time: 0,
            status_context: String::new(),
            status_detail: String::new(),
            directory: String::new(),
            created_at: 0.0,
            transcript_path: String::new(),
            tool: "claude".into(),
            background: 0,
            background_log_file: String::new(),
            tcp_mode: 0,
            wait_timeout: None,
            subagent_timeout: None,
            hints: None,
            origin_device_id: None,
            pid: None,
            launch_args: None,
            terminal_preset_requested: None,
            terminal_preset_effective: None,
            launch_context: None,
            name_announced: 0,
            running_tasks: None,
            idle_since: None,
        }
    }

    #[test]
    fn test_hamming_distance_with_many_alive() {
        let mut alive = HashSet::new();
        alive.insert("luna".to_string());
        alive.insert("nova".to_string());
        alive.insert("miso".to_string());
        alive.insert("kira".to_string());
        alive.insert("duma".to_string());

        assert!(is_too_similar("lina", &alive));
        assert!(is_too_similar("nava", &alive));
        assert!(!is_too_similar("bize", &alive));
    }

    #[test]
    fn test_hamming_distance_different_lengths() {
        let mut alive = HashSet::new();
        alive.insert("luna".to_string());

        assert!(!is_too_similar("lu", &alive));
        assert!(!is_too_similar("lunaa", &alive));
        assert!(!is_too_similar("l", &alive));
    }
}
