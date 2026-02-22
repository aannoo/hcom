//! Simple file-based logging for hcom
//!
//! Logs to ~/.hcom/.tmp/logs/hcom.log (same as Python hcom)
//! Uses JSONL format matching Python schema exactly:
//! - ISO 8601 timestamps (not Unix epoch)
//! - "subsystem" field (not "component")

use crate::config::Config;
use chrono::Utc;
use serde::Serialize;
use std::fs::{OpenOptions, create_dir_all};
use std::io::Write;

/// Log entry structure for safe JSON serialization
#[derive(Serialize)]
struct LogEntry<'a> {
    ts: String,
    level: String,
    subsystem: &'a str,
    event: &'a str,
    instance: String,
    msg: &'a str,
}

/// Log a message to the hcom log file
/// Uses Python-compatible schema: ts (ISO), level, subsystem, event, instance, msg
pub fn log(level: &str, subsystem: &str, event: &str, message: &str) {
    let path = crate::paths::log_path();

    // Ensure directory exists
    if let Some(parent) = path.parent() {
        let _ = create_dir_all(parent);
    }

    // ISO timestamp matching Python format
    let timestamp = Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let instance = Config::get().instance_name.unwrap_or_default();

    let entry = LogEntry {
        ts: timestamp,
        level: level.to_uppercase(),
        subsystem,
        event,
        instance,
        msg: message,
    };

    // Serialize with serde_json for proper escaping
    let log_line = match serde_json::to_string(&entry) {
        Ok(line) => line,
        Err(_) => return, // Silently fail on serialization error
    };

    // Append to file
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{}", log_line);
    }
}

/// Log info message
pub fn log_info(component: &str, event: &str, message: &str) {
    log("info", component, event, message);
}

/// Log warning message
pub fn log_warn(component: &str, event: &str, message: &str) {
    log("warn", component, event, message);
}

/// Log error message
pub fn log_error(component: &str, event: &str, message: &str) {
    log("error", component, event, message);
}
