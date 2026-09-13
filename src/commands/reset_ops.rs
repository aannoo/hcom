use std::fs;

use crate::db::HcomDb;
use crate::paths::{ARCHIVE_DIR, FLAGS_DIR, LAUNCH_DIR, LOGS_DIR, hcom_dir};
use crate::shared::shorten_path;

/// Get timestamp for archive directory names.
pub(crate) fn get_archive_timestamp() -> String {
    chrono::Local::now().format("%Y-%m-%d_%H%M%S").to_string()
}

/// Archive the current database to ~/.hcom/archive/session-{timestamp}/.
pub(crate) fn archive_and_clear_db() -> Result<Option<String>, String> {
    let base = hcom_dir();
    let db_file = base.join("hcom.db");
    let db_wal = base.join("hcom.db-wal");
    let db_shm = base.join("hcom.db-shm");

    if !db_file.exists() {
        return Ok(None);
    }

    let has_content = {
        let conn = rusqlite::Connection::open(&db_file).map_err(|e| e.to_string())?;
        let event_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap_or(0);
        let instance_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM instances", [], |r| r.get(0))
            .unwrap_or(0);
        event_count > 0 || instance_count > 0
    };

    if !has_content {
        remove_database_files(&db_file, &db_wal, &db_shm)?;
        return Ok(None);
    }

    if let Ok(conn) = rusqlite::Connection::open(&db_file) {
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
    }

    let timestamp = get_archive_timestamp();
    let session_archive = base.join(ARCHIVE_DIR).join(format!("session-{timestamp}"));
    fs::create_dir_all(&session_archive).map_err(|e| e.to_string())?;

    fs::copy(&db_file, session_archive.join("hcom.db")).map_err(|e| e.to_string())?;
    if db_wal.exists() {
        let _ = fs::copy(&db_wal, session_archive.join("hcom.db-wal"));
    }
    if db_shm.exists() {
        let _ = fs::copy(&db_shm, session_archive.join("hcom.db-shm"));
    }

    remove_database_files(&db_file, &db_wal, &db_shm)?;

    Ok(Some(session_archive.to_string_lossy().to_string()))
}

fn remove_database_files(
    db_file: &std::path::Path,
    db_wal: &std::path::Path,
    db_shm: &std::path::Path,
) -> Result<(), String> {
    // Remove sidecars first and the primary DB last. If a sidecar is locked,
    // leave the primary database intact rather than creating a partial reset.
    for path in [db_wal, db_shm, db_file] {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(format!(
                    "could not remove {}: {err}. Stop other hcom processes using this database and retry",
                    path.display()
                ));
            }
        }
    }
    Ok(())
}

/// Clean temp files (launch scripts, prompts, old logs).
pub(crate) fn clean_temp_files() {
    let base = hcom_dir();
    let cutoff_24h = crate::shared::time::now_epoch_f64() - 86400.0;
    let cutoff_30d = crate::shared::time::now_epoch_f64() - 30.0 * 86400.0;

    let launch_dir = base.join(LAUNCH_DIR);
    if launch_dir.exists()
        && let Ok(rd) = fs::read_dir(&launch_dir)
    {
        for entry in rd.filter_map(|e| e.ok()) {
            if entry.path().is_file()
                && let Ok(meta) = entry.metadata()
                && let Ok(mtime) = meta.modified()
            {
                let secs = crate::shared::system_time_to_epoch_f64(mtime);
                if secs < cutoff_24h {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }

    let prompts_dir = base.join(".tmp").join("prompts");
    if prompts_dir.exists()
        && let Ok(rd) = fs::read_dir(&prompts_dir)
    {
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md")
                && let Ok(meta) = entry.metadata()
                && let Ok(mtime) = meta.modified()
            {
                let secs = crate::shared::system_time_to_epoch_f64(mtime);
                if secs < cutoff_24h {
                    let _ = fs::remove_file(path);
                }
            }
        }
    }

    let logs_dir = base.join(LOGS_DIR);
    if logs_dir.exists()
        && let Ok(rd) = fs::read_dir(&logs_dir)
    {
        for entry in rd.filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with("background_")
                && name.ends_with(".log")
                && let Ok(meta) = entry.metadata()
                && let Ok(mtime) = meta.modified()
            {
                let secs = crate::shared::system_time_to_epoch_f64(mtime);
                if secs < cutoff_30d {
                    let _ = fs::remove_file(path);
                }
            }
        }
    }
}

/// Archive and reset config files.
pub(crate) fn reset_config() -> i32 {
    let base = hcom_dir();
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let archive_config_dir = base.join(ARCHIVE_DIR).join("config");

    let mut archived = false;

    let toml_path = base.join("config.toml");
    if toml_path.exists() {
        let _ = fs::create_dir_all(&archive_config_dir);
        if fs::copy(
            &toml_path,
            archive_config_dir.join(format!("config.toml.{timestamp}")),
        )
        .is_ok()
        {
            let _ = fs::remove_file(&toml_path);
            println!("Config archived to archive/config/config.toml.{timestamp}");
            archived = true;
        }
    }

    let env_path = base.join("config.env");
    if env_path.exists() {
        let _ = fs::create_dir_all(&archive_config_dir);
        if fs::copy(
            &env_path,
            archive_config_dir.join(format!("env.{timestamp}")),
        )
        .is_ok()
        {
            let _ = fs::remove_file(&env_path);
            if !archived {
                println!("Env archived to archive/config/env.{timestamp}");
            }
        }
    }

    if !archived {
        println!("No config file to reset");
    }
    0
}

pub(crate) fn clear_full_reset_artifacts() {
    let pidtrack = hcom_dir().join(".tmp").join("launched_pids.json");
    let _ = fs::remove_file(pidtrack);

    let device_id_file = hcom_dir().join(".tmp").join("device_id");
    let _ = fs::remove_file(&device_id_file);

    let instance_count_file = hcom_dir().join(FLAGS_DIR).join("instance_count");
    let _ = fs::remove_file(&instance_count_file);
}

pub(crate) fn bootstrap_fresh_db() {
    if let Ok(fresh_db) = HcomDb::open() {
        let _ = fresh_db.init_db();
        let _ = fresh_db.log_reset_event();
    }
}

pub(crate) fn print_archive_result(result: Result<Option<String>, String>) -> i32 {
    match result {
        Ok(Some(path)) => {
            println!("Archived to {}/", shorten_path(&path));
            println!("Started fresh HCOM conversation");
            0
        }
        Ok(None) => {
            println!("No HCOM conversation to clear");
            0
        }
        Err(e) => {
            eprintln!("Error: Failed to archive: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_archive_timestamp_format() {
        let ts = get_archive_timestamp();
        assert!(ts.len() >= 15);
        assert!(ts.contains('-'));
        assert!(ts.contains('_'));
    }

    #[test]
    fn remove_database_files_reports_failure_instead_of_claiming_reset() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("hcom.db");
        let wal_path = dir.path().join("hcom.db-wal");
        let shm_path = dir.path().join("hcom.db-shm");
        std::fs::create_dir(&db_path).unwrap();

        let err = remove_database_files(&db_path, &wal_path, &shm_path).unwrap_err();
        assert!(err.contains("could not remove"));
        assert!(err.contains("Stop other hcom processes"));
    }

    #[cfg(windows)]
    #[test]
    #[serial_test::serial]
    fn test_windows_reset_archive_handles_command_owned_live_connection() {
        let (_tmp, hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();

        let db = HcomDb::open().expect("failed to open test database");
        db.conn()
            .execute(
                "INSERT INTO events (timestamp, type, instance, data) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    "2026-09-04T00:00:00Z",
                    "message",
                    "test_instance",
                    "{\"content\":\"test\"}"
                ],
            )
            .expect("failed to insert test event");

        let wal_path = hcom_dir.join("hcom.db-wal");
        assert!(
            wal_path.exists(),
            "expected non-empty WAL database with active WAL sidecar"
        );

        // Command-owned handle flow: cmd_reset takes ownership of `db`, releases it,
        // archives and clears the database, and bootstraps a fresh database.
        let ctx = crate::shared::CommandContext {
            explicit_name: None,
            identity: None,
            go: true,
        };
        let reset_args = crate::commands::reset::ResetArgs { target: None };
        let exit_code = crate::commands::reset::cmd_reset(db, &reset_args, Some(&ctx));
        assert_eq!(
            exit_code, 0,
            "cmd_reset must succeed when command-owned handle is released"
        );

        // 1. Archive is produced and readable
        let archive_base = hcom_dir.join("archive");
        assert!(archive_base.exists(), "archive directory must exist");
        let mut archived_sessions = std::fs::read_dir(&archive_base)
            .expect("must read archive dir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.starts_with("session-"))
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            archived_sessions.len(),
            1,
            "expected exactly one archived session directory"
        );
        let session_dir = archived_sessions.pop().unwrap().path();
        let archived_db_path = session_dir.join("hcom.db");
        assert!(
            archived_db_path.exists(),
            "archived hcom.db file must exist"
        );

        // 2. Original event exists in archive
        let archive_conn = rusqlite::Connection::open(&archived_db_path)
            .expect("archived database must be queryable");
        let archived_event_count: i64 = archive_conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'message' AND instance = 'test_instance'",
                [],
                |r| r.get(0),
            )
            .expect("must query archived events");
        assert_eq!(
            archived_event_count, 1,
            "archived database must contain original event"
        );

        // 3. Fresh active DB can be initialized after reset
        let fresh_db = HcomDb::open().expect("must open fresh database after reset");
        let fresh_message_count: i64 = fresh_db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'message'",
                [],
                |r| r.get(0),
            )
            .expect("must query fresh events");
        assert_eq!(
            fresh_message_count, 0,
            "fresh database must have cleared previous messages"
        );
        let reset_event_count: i64 = fresh_db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'life' AND instance = '_device'",
                [],
                |r| r.get(0),
            )
            .expect("must query life/_device reset event");
        assert_eq!(
            reset_event_count, 1,
            "fresh database must contain initial life/_device reset event"
        );

        let reset_data_raw: String = fresh_db
            .conn()
            .query_row(
                "SELECT data FROM events WHERE type = 'life' AND instance = '_device' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .expect("must query reset event data");
        let reset_data: serde_json::Value =
            serde_json::from_str(&reset_data_raw).expect("reset event data must be valid JSON");
        assert_eq!(
            reset_data.get("action").and_then(|v| v.as_str()),
            Some("reset"),
            "life/_device event action must be 'reset'"
        );

        let relay_reset_ts = fresh_db
            .kv_get("relay_local_reset_ts")
            .expect("must query relay_local_reset_ts")
            .expect("relay_local_reset_ts must be present after reset");
        assert!(
            !relay_reset_ts.trim().is_empty(),
            "relay_local_reset_ts must not be empty"
        );
    }

    #[cfg(windows)]
    #[test]
    #[serial_test::serial]
    fn test_windows_reset_fails_safely_on_external_live_handle() {
        let (_tmp, hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();

        let db = HcomDb::open().expect("failed to open test database");
        db.conn()
            .execute(
                "INSERT INTO events (timestamp, type, instance, data) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    "2026-09-04T00:00:00Z",
                    "message",
                    "external_instance",
                    "{\"content\":\"external\"}"
                ],
            )
            .expect("failed to insert test event");

        let db_path = hcom_dir.join("hcom.db");

        // Simulate an uncooperative external process holding an open SQLite connection
        let external_conn =
            rusqlite::Connection::open(&db_path).expect("failed to open external connection");
        let pre_event_count: i64 = external_conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'message'",
                [],
                |r| r.get(0),
            )
            .expect("external connection must read initial state");
        assert_eq!(
            pre_event_count, 1,
            "external connection must participate in current DB/WAL state"
        );

        let ctx = crate::shared::CommandContext {
            explicit_name: None,
            identity: None,
            go: true,
        };
        let reset_args = crate::commands::reset::ResetArgs { target: None };
        let exit_code = crate::commands::reset::cmd_reset(db, &reset_args, Some(&ctx));
        assert_ne!(
            exit_code, 0,
            "reset must fail when external process holds open handle"
        );

        // Verify original database file remains present and queryable through the external handle
        assert!(
            db_path.exists(),
            "original database file must remain present when external handle blocks reset"
        );
        let event_count: i64 = external_conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'message'",
                [],
                |r| r.get(0),
            )
            .expect("original database must remain readable through external connection");
        assert_eq!(
            event_count, 1,
            "original database content must remain intact and not mutated by reset failure"
        );

        // Also independently verify through a fresh connection that on-disk DB is uncorrupted
        let fresh_conn = rusqlite::Connection::open(&db_path)
            .expect("failed to open fresh connection to original db");
        let fresh_count: i64 = fresh_conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'message'",
                [],
                |r| r.get(0),
            )
            .expect("fresh connection must read preserved original events");
        assert_eq!(
            fresh_count, 1,
            "original database on disk must remain queryable with message count = 1"
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn test_unix_reset_handles_command_owned_connection() {
        let (_tmp, hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();

        let db = HcomDb::open().expect("failed to open test database");
        db.conn()
            .execute(
                "INSERT INTO events (timestamp, type, instance, data) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    "2026-09-04T00:00:00Z",
                    "message",
                    "test_instance",
                    "{\"content\":\"test\"}"
                ],
            )
            .expect("failed to insert test event");

        let ctx = crate::shared::CommandContext {
            explicit_name: None,
            identity: None,
            go: true,
        };
        let reset_args = crate::commands::reset::ResetArgs { target: None };
        let exit_code = crate::commands::reset::cmd_reset(db, &reset_args, Some(&ctx));
        assert_eq!(exit_code, 0);

        let archive_base = hcom_dir.join("archive");
        assert!(archive_base.exists());

        let fresh_db = HcomDb::open().expect("must open fresh database after reset");
        let fresh_message_count: i64 = fresh_db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'message'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fresh_message_count, 0);
    }
}
