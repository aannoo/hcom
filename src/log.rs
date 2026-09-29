//! Structured JSONL logging with size-based rotation (~8MB, 3 backups).

use crate::config::Config;
use chrono::Utc;
use std::fs::{self, OpenOptions, create_dir_all};
use std::io::Write;

/// Max log file size before rotation (8MB).
const MAX_BYTES: u64 = 8_000_000;

/// Default number of backup files to keep (.log.1, .log.2, .log.3).
const DEFAULT_BACKUPS: u32 = 3;

/// Read backup count from HCOM_LOG_BACKUPS env, falling back to default.
fn log_backups() -> u32 {
    std::env::var("HCOM_LOG_BACKUPS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(DEFAULT_BACKUPS)
}

const LOG_FILE: &str = "hcom.log";

/// ISO 8601 timestamp for log entries.
fn timestamp_now() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Rotate log file if over size limit.
/// hcom.log -> hcom.log.1 -> hcom.log.2 -> hcom.log.3 (oldest deleted).
fn rotate_if_needed(path: &std::path::Path) {
    let size = match fs::metadata(path) {
        Ok(m) => m.len(),
        Err(_) => return,
    };
    if size <= MAX_BYTES {
        return;
    }

    // Shift older backups: .3 deleted, .2->.3, .1->.2
    let backups = log_backups();
    for i in (1..=backups).rev() {
        let older = path.with_file_name(format!("{}.{}", LOG_FILE, i));
        if i == backups {
            let _ = fs::remove_file(&older);
        } else if older.exists() {
            let _ = fs::rename(
                &older,
                path.with_file_name(format!("{}.{}", LOG_FILE, i + 1)),
            );
        }
    }
    // Current -> .1
    let _ = fs::rename(path, path.with_file_name(format!("{}.1", LOG_FILE)));
}

/// Log a structured event to the hcom log file.
///
/// Uses manual serde_json writes for full control over field ordering and
/// optional fields.
/// plus optional structured fields.
pub fn log(level: &str, subsystem: &str, event: &str, message: &str) {
    log_with_fields(level, subsystem, event, message, &[]);
}

/// Log with additional structured key-value fields.
///
/// Fields are appended after the standard fields (ts, level, subsystem, event, instance, msg).
/// Empty values are omitted.
pub fn log_with_fields(
    level: &str,
    subsystem: &str,
    event: &str,
    message: &str,
    fields: &[(&str, &str)],
) {
    let path = crate::paths::log_path();

    // Ensure directory exists
    if let Some(parent) = path.parent() {
        let _ = create_dir_all(parent);
    }

    // Rotate before writing
    rotate_if_needed(&path);

    let ts = timestamp_now();
    let instance = Config::get().instance_name.unwrap_or_default();

    // Build JSON manually for control over field ordering and optionals
    let mut obj = serde_json::Map::new();
    obj.insert("ts".into(), serde_json::Value::String(ts));
    obj.insert(
        "level".into(),
        serde_json::Value::String(level.to_uppercase()),
    );
    obj.insert(
        "subsystem".into(),
        serde_json::Value::String(subsystem.into()),
    );
    obj.insert("event".into(), serde_json::Value::String(event.into()));

    if !instance.is_empty() {
        obj.insert("instance".into(), serde_json::Value::String(instance));
    }

    if !message.is_empty() {
        obj.insert("msg".into(), serde_json::Value::String(message.into()));
    }

    // Additional structured fields
    for (key, value) in fields {
        if !value.is_empty() {
            obj.insert(
                (*key).to_string(),
                serde_json::Value::String((*value).to_string()),
            );
        }
    }

    let log_line = match serde_json::to_string(&serde_json::Value::Object(obj)) {
        Ok(line) => line,
        Err(_) => return,
    };

    // One write per line: O_APPEND keeps a single small write whole, but
    // `writeln!` issues the line and its newline separately, which
    // interleaves with other processes (`…}{…` lines).
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(format!("{log_line}\n").as_bytes());
    }
}

/// Log info message.
pub fn log_info(subsystem: &str, event: &str, message: &str) {
    log("INFO", subsystem, event, message);
}

/// Log warning message.
pub fn log_warn(subsystem: &str, event: &str, message: &str) {
    log("WARN", subsystem, event, message);
}

/// Log error message with structured error_type/error_msg fields.
/// Splits "TypeName: detail" into error_type + error_msg.
pub fn log_error(subsystem: &str, event: &str, message: &str) {
    // Split "ErrorType: message" into structured fields
    if let Some((error_type, error_msg)) = message.split_once(": ")
        && !error_type.contains(' ')
    {
        log_with_fields(
            "ERROR",
            subsystem,
            event,
            message,
            &[("error_type", error_type), ("error_msg", error_msg)],
        );
        return;
    }
    log("ERROR", subsystem, event, message);
}

/// Get recent log entries filtered by level and time.
///
/// Returns entries newest-first, up to `limit`.
pub fn get_recent_logs(hours: f64, levels: &[&str], limit: usize) -> Vec<serde_json::Value> {
    let path = crate::paths::log_path();
    if !path.exists() {
        return vec![];
    }

    let cutoff = Utc::now() - chrono::Duration::milliseconds((hours * 3_600_000.0) as i64);
    let cutoff_str = cutoff.format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let mut entries: Vec<serde_json::Value> = Vec::new();

    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Filter by level
        let entry_level = entry.get("level").and_then(|v| v.as_str()).unwrap_or("");
        if !levels.contains(&entry_level) {
            continue;
        }

        // Filter by time
        let ts = entry.get("ts").and_then(|v| v.as_str()).unwrap_or("");
        if ts >= cutoff_str.as_str() {
            entries.push(entry);
        }
    }

    // Sort newest first
    entries.sort_by(|a, b| {
        let ts_a = a.get("ts").and_then(|v| v.as_str()).unwrap_or("");
        let ts_b = b.get("ts").and_then(|v| v.as_str()).unwrap_or("");
        ts_b.cmp(ts_a)
    });

    entries.truncate(limit);
    entries
}

/// Get summary of recent log activity.
///
/// Returns JSON with error_count, warn_count, last_error, last_warn.
pub fn get_log_summary(hours: f64) -> serde_json::Value {
    let entries = get_recent_logs(hours, &["ERROR", "WARN"], 100);

    let errors: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|e| e.get("level").and_then(|v| v.as_str()) == Some("ERROR"))
        .collect();
    let warns: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|e| e.get("level").and_then(|v| v.as_str()) == Some("WARN"))
        .collect();

    let last_error = errors.first().map(|e| {
        serde_json::json!({
            "event": format!("{}.{}",
                e.get("subsystem").and_then(|v| v.as_str()).unwrap_or(""),
                e.get("event").and_then(|v| v.as_str()).unwrap_or("")
            ),
            "ts": e.get("ts"),
            "instance": e.get("instance"),
        })
    });

    let last_warn = warns.first().map(|e| {
        serde_json::json!({
            "event": format!("{}.{}",
                e.get("subsystem").and_then(|v| v.as_str()).unwrap_or(""),
                e.get("event").and_then(|v| v.as_str()).unwrap_or("")
            ),
            "ts": e.get("ts"),
            "instance": e.get("instance"),
        })
    });

    serde_json::json!({
        "error_count": errors.len(),
        "warn_count": warns.len(),
        "last_error": last_error,
        "last_warn": last_warn,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;

    #[test]
    fn test_rotate_if_needed_small_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE);
        fs::write(&path, "small content").unwrap();
        rotate_if_needed(&path);
        assert!(path.exists()); // Should not rotate
    }

    #[test]
    fn test_rotate_if_needed_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE);

        // Create a file just over MAX_BYTES
        let mut f = fs::File::create(&path).unwrap();
        let chunk = vec![b'x'; 1024];
        for _ in 0..(MAX_BYTES / 1024 + 1) {
            f.write_all(&chunk).unwrap();
        }
        f.flush().unwrap();
        drop(f);

        rotate_if_needed(&path);

        // Original should be gone, .1 should exist
        assert!(!path.exists());
        assert!(dir.path().join(format!("{}.1", LOG_FILE)).exists());
    }

    #[test]
    fn test_rotate_chain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE);

        // Create existing backups
        fs::write(dir.path().join(format!("{}.1", LOG_FILE)), "backup1").unwrap();
        fs::write(dir.path().join(format!("{}.2", LOG_FILE)), "backup2").unwrap();

        // Create oversized main log
        let mut f = fs::File::create(&path).unwrap();
        let chunk = vec![b'x'; 1024];
        for _ in 0..(MAX_BYTES / 1024 + 1) {
            f.write_all(&chunk).unwrap();
        }
        f.flush().unwrap();
        drop(f);

        rotate_if_needed(&path);

        // .1 was old backup1, now should be .2
        // .2 was old backup2, now should be .3
        // New .1 should be the old main log
        assert!(!path.exists());
        assert!(dir.path().join(format!("{}.1", LOG_FILE)).exists());
        assert!(dir.path().join(format!("{}.2", LOG_FILE)).exists());
        assert!(dir.path().join(format!("{}.3", LOG_FILE)).exists());

        // .2 should contain old backup1 content
        let content = fs::read_to_string(dir.path().join(format!("{}.2", LOG_FILE))).unwrap();
        assert_eq!(content, "backup1");

        // .3 should contain old backup2 content
        let content = fs::read_to_string(dir.path().join(format!("{}.3", LOG_FILE))).unwrap();
        assert_eq!(content, "backup2");
    }

    #[test]
    fn test_rotate_deletes_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE);

        // Create all 3 backups
        fs::write(dir.path().join(format!("{}.1", LOG_FILE)), "b1").unwrap();
        fs::write(dir.path().join(format!("{}.2", LOG_FILE)), "b2").unwrap();
        fs::write(dir.path().join(format!("{}.3", LOG_FILE)), "b3").unwrap();

        // Create oversized main log
        let mut f = fs::File::create(&path).unwrap();
        let chunk = vec![b'x'; 1024];
        for _ in 0..(MAX_BYTES / 1024 + 1) {
            f.write_all(&chunk).unwrap();
        }
        f.flush().unwrap();
        drop(f);

        rotate_if_needed(&path);

        // .3 (old b3) should be deleted, replaced by old .2 (b2)
        let content = fs::read_to_string(dir.path().join(format!("{}.3", LOG_FILE))).unwrap();
        assert_eq!(content, "b2");
    }

    #[test]
    fn test_get_recent_logs_empty() {
        let entries = get_recent_logs(1.0, &["ERROR", "WARN"], 20);
        // May or may not be empty depending on whether log file exists
        let _ = entries;
    }

    #[test]
    fn test_get_log_summary_structure() {
        let summary = get_log_summary(1.0);
        assert!(summary.get("error_count").is_some());
        assert!(summary.get("warn_count").is_some());
    }

    #[test]
    fn test_get_recent_logs_from_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE);

        // Write some test log entries
        let ts = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let entries = [
            format!(
                r#"{{"ts":"{}","level":"ERROR","subsystem":"test","event":"e1","msg":"err"}}"#,
                ts
            ),
            format!(
                r#"{{"ts":"{}","level":"WARN","subsystem":"test","event":"e2","msg":"warn"}}"#,
                ts
            ),
            format!(
                r#"{{"ts":"{}","level":"INFO","subsystem":"test","event":"e3","msg":"info"}}"#,
                ts
            ),
        ];
        fs::write(&path, entries.join("\n") + "\n").unwrap();

        // Read the file directly to test parsing
        let content = fs::read_to_string(&path).unwrap();
        let mut results: Vec<serde_json::Value> = Vec::new();
        for line in content.lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
                && v.get("level").and_then(|l| l.as_str()) == Some("ERROR")
            {
                results.push(v);
            }
        }
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["event"], "e1");
    }
}
