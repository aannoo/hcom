//! Codex transcript watcher — monitors rollout JSONL for tool calls and user prompts.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use regex::Regex;
use serde_json::Value;

use crate::db::HcomDb;
use crate::log::{log_error, log_info};

/// Regex to extract file paths from apply_patch input
/// Matches: *** Update File: path, *** Add File: path, *** Delete File: path
fn apply_patch_regex() -> Regex {
    Regex::new(r"\*\*\* (?:Update|Add|Delete) File: (.+?)(?:\n|$)").unwrap()
}

/// Transcript watcher state
///
/// Only used for Codex — monitors rollout JSONL for apply_patch file edits.
/// Shell commands and prompts are handled by native hooks, so only file edits
/// are logged here.
pub struct TranscriptWatcher {
    instance_name: String,
    transcript_path: Option<String>,
    file_pos: u64,
    logged_call_ids: HashSet<String>,
    apply_patch_re: Regex,
}

impl TranscriptWatcher {
    /// Create a new transcript watcher for an instance
    pub fn new(instance_name: &str) -> Self {
        Self {
            instance_name: instance_name.to_string(),
            transcript_path: None,
            file_pos: 0,
            logged_call_ids: HashSet::new(),
            apply_patch_re: apply_patch_regex(),
        }
    }

    /// Update transcript path (may not be known at init)
    pub fn set_transcript_path(&mut self, path: &str) {
        if self.transcript_path.as_deref() != Some(path) {
            self.transcript_path = Some(path.to_string());
            self.file_pos = 0; // Reset position for new file
        }
    }

    /// Parse new transcript entries, log tool calls and prompts to events DB
    ///
    /// Returns number of file edits logged (apply_patch only).
    pub fn sync(&mut self, db: &HcomDb) -> u32 {
        let path = match &self.transcript_path {
            Some(p) => p.clone(),
            None => return 0,
        };

        let path = Path::new(&path);
        if !path.exists() {
            return 0;
        }

        let mut edits_logged = 0;

        // Check if file was truncated/replaced
        if let Ok(metadata) = path.metadata() {
            if metadata.len() < self.file_pos {
                self.file_pos = 0;
            }
        }

        // Open and seek to last position
        let file = match File::open(path) {
            Ok(f) => f,
            Err(_) => return 0,
        };

        let mut reader = BufReader::new(file);
        if reader.seek(SeekFrom::Start(self.file_pos)).is_err() {
            return 0;
        }

        // Read new lines, only advancing file_pos past complete lines
        let mut line = String::new();
        loop {
            line.clear();
            let pos_before = reader.stream_position().unwrap_or(self.file_pos);
            match reader.read_line(&mut line) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    if !line.ends_with('\n') {
                        // Partial line (writer still appending) — revert and retry next poll
                        self.file_pos = pos_before;
                        break;
                    }
                    if line.trim().is_empty() {
                        self.file_pos = reader.stream_position().unwrap_or(self.file_pos);
                        continue;
                    }
                    if let Ok(entry) = serde_json::from_str::<Value>(&line) {
                        edits_logged += self.process_entry(&entry, db);
                    }
                    self.file_pos = reader.stream_position().unwrap_or(self.file_pos);
                }
                Err(_) => break,
            }
        }

        edits_logged
    }

    /// Process a single transcript entry
    fn process_entry(&mut self, entry: &Value, db: &HcomDb) -> u32 {
        if entry.get("type").and_then(|v| v.as_str()) != Some("response_item") {
            return 0;
        }

        let payload = match entry.get("payload") {
            Some(p) => p,
            None => return 0,
        };

        let payload_type = payload.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let timestamp = entry
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // User prompt status is handled by native hooks (UserPromptSubmit)
        if payload_type == "message" {
            return 0;
        }

        // Handle function_call and custom_tool_call
        if payload_type != "function_call" && payload_type != "custom_tool_call" {
            return 0;
        }

        let tool_name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let call_id = payload
            .get("call_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Skip if already processed
        if !call_id.is_empty() && self.logged_call_ids.contains(call_id) {
            return 0;
        }

        let mut edits = 0;

        if tool_name == "apply_patch" {
            // Extract file paths from apply_patch input
            let input_text = payload
                .get("input")
                .or_else(|| payload.get("arguments"))
                .and_then(|v| v.as_str())
                .unwrap_or("");

            for caps in self.apply_patch_re.captures_iter(input_text) {
                if let Some(filepath) = caps.get(1) {
                    self.log_file_edit(filepath.as_str().trim(), timestamp, db);
                    edits += 1;
                }
            }
        }
        // Shell commands are handled by native hooks (PreToolUse)

        // Track call_id to avoid duplicates
        if !call_id.is_empty() {
            // Bound memory: clear when too large
            if self.logged_call_ids.len() > 10000 {
                self.logged_call_ids.clear();
            }
            self.logged_call_ids.insert(call_id.to_string());
        }

        edits
    }

    /// Log a file edit status event for collision detection
    fn log_file_edit(&self, filepath: &str, timestamp: &str, db: &HcomDb) {
        if let Err(e) = db.log_status_event(
            &self.instance_name,
            "active",
            "tool:apply_patch",
            Some(filepath),
            if timestamp.is_empty() {
                None
            } else {
                Some(timestamp)
            },
        ) {
            log_error(
                "transcript",
                "log_event.fail",
                &format!("Failed to log file edit: {}", e),
            );
        }
    }
}

/// Run transcript watcher loop in a thread
///
/// Polls every 5 seconds until running flag is cleared.
pub fn run_transcript_watcher(
    running: Arc<AtomicBool>,
    instance_name: String,
    poll_interval: Duration,
) {
    log_info(
        "transcript",
        "watcher.start",
        &format!("Starting transcript watcher for {}", instance_name),
    );

    let mut watcher = TranscriptWatcher::new(&instance_name);

    // Open database
    let db = match HcomDb::open() {
        Ok(db) => db,
        Err(e) => {
            log_error(
                "transcript",
                "db.open.fail",
                &format!("Failed to open DB: {}", e),
            );
            return;
        }
    };

    while running.load(Ordering::Acquire) {
        // Get transcript path from instance DB (may be set by notify hook)
        match db.get_transcript_path(&instance_name) {
            Ok(Some(path)) => {
                watcher.set_transcript_path(&path);
            }
            Ok(None) => {
                // No transcript path set - normal case
            }
            Err(e) => {
                log_error(
                    "native",
                    "transcript.init",
                    &format!("DB error getting transcript path: {}", e),
                );
            }
        }

        // Sync any new entries
        let edits = watcher.sync(&db);
        if edits > 0 {
            log_info(
                "transcript",
                "watcher.sync",
                &format!("Logged {} file edits for {}", edits, instance_name),
            );
        }

        // Sleep in small increments to check running flag
        let mut remaining = poll_interval;
        while running.load(Ordering::Acquire) && remaining > Duration::ZERO {
            let sleep_time = remaining.min(Duration::from_millis(500));
            std::thread::sleep(sleep_time);
            remaining = remaining.saturating_sub(sleep_time);
        }
    }

    log_info(
        "transcript",
        "watcher.stop",
        &format!("Transcript watcher stopped for {}", instance_name),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn watcher() -> TranscriptWatcher {
        TranscriptWatcher::new("test")
    }

    fn setup_test_db() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let temp_dir = std::env::temp_dir();
        let test_id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let db_path = temp_dir.join(format!(
            "test_hcom_transcript_{}_{}.db",
            std::process::id(),
            test_id
        ));
        let _ = Connection::open(&db_path).unwrap();
        db_path
    }

    fn cleanup_test_db(path: PathBuf) {
        let _ = std::fs::remove_file(path);
    }

    // ---- apply_patch_regex ----

    #[test]
    fn regex_matches_update_add_delete() {
        let re = apply_patch_regex();
        let input = "*** Update File: src/main.rs\n*** Add File: new.rs\n*** Delete File: old.rs\n";
        let paths: Vec<&str> = re
            .captures_iter(input)
            .filter_map(|c| c.get(1).map(|m| m.as_str()))
            .collect();
        assert_eq!(paths, vec!["src/main.rs", "new.rs", "old.rs"]);
    }

    #[test]
    fn regex_no_match() {
        let re = apply_patch_regex();
        let input = "nothing relevant here";
        assert_eq!(re.captures_iter(input).count(), 0);
    }

    #[test]
    fn regex_end_of_string_without_newline() {
        let re = apply_patch_regex();
        let input = "*** Update File: path/to/file.py";
        let caps = re.captures(input).unwrap();
        assert_eq!(caps.get(1).unwrap().as_str(), "path/to/file.py");
    }

    // ---- deduplication ----

    #[test]
    fn logged_call_ids_bounds_memory() {
        let db_path = setup_test_db();
        let db = crate::db::HcomDb::open_raw(&db_path).unwrap();
        let mut w = watcher();

        for i in 0..10001 {
            w.logged_call_ids.insert(format!("id_{}", i));
        }

        let entry = json!({
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "noop",
                "call_id": "new_id"
            }
        });
        let edits = w.process_entry(&entry, &db);

        assert_eq!(edits, 0);
        assert_eq!(w.logged_call_ids.len(), 1);
        assert!(w.logged_call_ids.contains("new_id"));

        cleanup_test_db(db_path);
    }
}
