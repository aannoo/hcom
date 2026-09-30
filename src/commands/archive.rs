//! `hcom archive` command — list and query archived sessions.
//!
//!
//! Archives are stored in `~/.hcom/archive/session-{timestamp}/`, each containing
//! a copy of hcom.db from that session. This command lists and queries them.

use std::path::{Path, PathBuf};

use crate::db::HcomDb;
use crate::paths::{ARCHIVE_DIR, hcom_dir};
use crate::shared::CommandContext;

/// Parsed arguments for `hcom archive`.
#[derive(clap::Parser, Debug)]
#[command(name = "archive", about = "List and query archived sessions")]
pub struct ArchiveArgs {
    /// Archive selector (index or name prefix)
    pub selector: Option<String>,
    /// Subcommand (e.g. "agents")
    pub subcmd: Option<String>,
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Filter to current directory
    #[arg(long)]
    pub here: bool,
    /// SQL WHERE clause for filtering
    #[arg(long)]
    pub sql: Option<String>,
    /// Limit archives listed (default: all), or events queried from a
    /// selected archive (default: 20)
    #[arg(long)]
    pub last: Option<usize>,
}

/// Get list of archive sessions with metadata.
fn list_archives(here_filter: bool) -> Vec<serde_json::Value> {
    let archive_dir = hcom_dir().join(ARCHIVE_DIR);
    if !archive_dir.exists() {
        return Vec::new();
    }

    let cwd = if here_filter {
        std::env::current_dir()
            .ok()
            .map(|p| p.to_string_lossy().to_string())
    } else {
        None
    };

    let mut session_dirs: Vec<PathBuf> = std::fs::read_dir(&archive_dir)
        .ok()
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.is_dir()
                        && p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with("session-"))
                })
                .collect()
        })
        .unwrap_or_default();

    // Sort newest first
    session_dirs.sort_by(|a, b| b.cmp(a));

    let mut archives = Vec::new();

    for session_dir in &session_dirs {
        let db_path = session_dir.join("hcom.db");
        if !db_path.exists() {
            continue;
        }

        let dir_name = session_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        let timestamp = dir_name.strip_prefix("session-").unwrap_or("").to_string();

        let (event_count, instance_count) =
            match query_archive_counts(&db_path, cwd.as_deref(), here_filter) {
                Some((e, i)) => (Some(e), Some(i)),
                None => continue, // filtered out by --here, or error
            };

        // Get file metadata
        let (size_bytes, created) = match std::fs::metadata(&db_path) {
            Ok(meta) => {
                let size = meta.len() as i64;
                let mtime = meta
                    .modified()
                    .map(crate::shared::system_time_to_epoch_f64)
                    .unwrap_or(0.0);
                (size, mtime)
            }
            Err(_) => (0, 0.0),
        };

        archives.push(serde_json::json!({
            "index": archives.len() + 1,
            "name": dir_name,
            "path": session_dir.to_string_lossy(),
            "timestamp": timestamp,
            "size_bytes": size_bytes,
            "created": created,
            "events": event_count,
            "instances": instance_count,
        }));
    }

    // Renumber after filtering
    for (i, a) in archives.iter_mut().enumerate() {
        a["index"] = serde_json::json!(i + 1);
    }

    archives
}

fn limit_archives(archives: &[serde_json::Value], last: Option<usize>) -> &[serde_json::Value] {
    &archives[..archives.len().min(last.unwrap_or(usize::MAX))]
}

/// Query event/instance counts from an archive DB. Returns None if filtered out.
fn query_archive_counts(
    db_path: &Path,
    cwd: Option<&str>,
    here_filter: bool,
) -> Option<(i64, i64)> {
    let conn = rusqlite::Connection::open(db_path).ok()?;
    let event_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap_or(0);
    let instance_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM instances", [], |r| r.get(0))
        .unwrap_or(0);

    if here_filter && let Some(cwd) = cwd {
        let has_match: bool = conn
            .query_row(
                "SELECT 1 FROM instances WHERE directory = ?1 LIMIT 1",
                rusqlite::params![cwd],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if !has_match {
            return None;
        }
    }

    Some((event_count, instance_count))
}

/// Resolve archive by index (1-based) or name prefix.
fn resolve_archive<'a>(
    selector: &str,
    archives: &'a [serde_json::Value],
) -> Option<&'a serde_json::Value> {
    // Try as index
    if let Ok(idx) = selector.parse::<usize>()
        && idx >= 1
        && idx <= archives.len()
    {
        return Some(&archives[idx - 1]);
    }

    // Try as name or prefix match
    for archive in archives {
        let name = archive["name"].as_str().unwrap_or("");
        if name == selector || name.contains(selector) {
            return Some(archive);
        }
    }

    None
}

/// Query events from an archive database.
fn query_archive_events(
    archive: &serde_json::Value,
    sql_filter: Option<&str>,
    last: usize,
) -> Result<Vec<serde_json::Value>, String> {
    let path = archive["path"].as_str().ok_or("Invalid archive path")?;
    let db_path = PathBuf::from(path).join("hcom.db");
    let conn = rusqlite::Connection::open(&db_path).map_err(|e| e.to_string())?;

    let query = if let Some(filter) = sql_filter {
        // Try events_v view first
        let has_view = conn
            .query_row("SELECT 1 FROM events_v LIMIT 1", [], |_| Ok(true))
            .is_ok();
        if has_view {
            format!(
                "SELECT id, timestamp, type, instance, data FROM events_v WHERE {filter} ORDER BY id DESC LIMIT {last}"
            )
        } else {
            format!(
                "SELECT id, timestamp, type, instance, data FROM events WHERE {filter} ORDER BY id DESC LIMIT {last}"
            )
        }
    } else {
        format!(
            "SELECT id, timestamp, type, instance, data FROM events ORDER BY id DESC LIMIT {last}"
        )
    };

    let mut stmt = conn.prepare(&query).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            let id: i64 = row.get(0)?;
            let timestamp: String = row.get(1)?;
            let etype: String = row.get(2)?;
            let instance: String = row.get(3)?;
            let data_str: String = row.get::<_, String>(4).unwrap_or_default();
            let data: serde_json::Value =
                serde_json::from_str(&data_str).unwrap_or(serde_json::json!({}));
            Ok(serde_json::json!({
                "id": id,
                "timestamp": timestamp,
                "type": etype,
                "instance": instance,
                "data": data,
            }))
        })
        .map_err(|e| e.to_string())?;

    let mut events: Vec<serde_json::Value> = rows.filter_map(|r| r.ok()).collect();
    events.reverse(); // Show oldest first
    Ok(events)
}

/// Query instances from an archive database.
fn query_archive_instances(
    archive: &serde_json::Value,
    sql_filter: Option<&str>,
) -> Result<Vec<serde_json::Value>, String> {
    let path = archive["path"].as_str().ok_or("Invalid archive path")?;
    let db_path = PathBuf::from(path).join("hcom.db");
    let conn = rusqlite::Connection::open(&db_path).map_err(|e| e.to_string())?;

    let query = if let Some(filter) = sql_filter {
        format!(
            "SELECT name, status, directory, transcript_path, session_id FROM instances WHERE {filter} ORDER BY created_at DESC"
        )
    } else {
        "SELECT name, status, directory, transcript_path, session_id FROM instances ORDER BY created_at DESC".to_string()
    };

    let mut stmt = conn.prepare(&query).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "name": row.get::<_, String>(0).unwrap_or_default(),
                "status": row.get::<_, String>(1).unwrap_or_default(),
                "directory": row.get::<_, String>(2).unwrap_or_default(),
                "transcript_path": row.get::<_, String>(3).unwrap_or_default(),
                "session_id": row.get::<_, String>(4).unwrap_or_default(),
            }))
        })
        .map_err(|e| e.to_string())?;

    Ok(rows.filter_map(|r| r.ok()).collect())
}

use crate::shared::shorten_path;

pub fn cmd_archive(_db: &HcomDb, args: &ArchiveArgs, _ctx: Option<&CommandContext>) -> i32 {
    let json_output = args.json;
    let here_filter = args.here;
    let last_count = args.last;
    let sql_filter = args.sql.clone();

    // Get archives list
    let archives = list_archives(here_filter);

    // No selector = list mode
    if args.selector.is_none() {
        if archives.is_empty() {
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({"archives": [], "count": 0, "total": 0})
                );
            } else {
                println!("No archives found");
            }
            return 0;
        }

        let visible_archives = limit_archives(&archives, last_count);

        if json_output {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "archives": visible_archives,
                    "count": visible_archives.len(),
                    "total": archives.len(),
                }))
                .unwrap_or_default()
            );
            return 0;
        }

        // Human-readable list
        println!("Archives:");
        for archive in visible_archives {
            let idx = archive["index"].as_i64().unwrap_or(0);
            let name = archive["name"].as_str().unwrap_or("?");
            let events = archive["events"]
                .as_i64()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into());
            let instances = archive["instances"]
                .as_i64()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into());
            println!("  {idx:>2}. {name}  {events} events  {instances} agents");
        }
        if visible_archives.len() < archives.len() {
            println!(
                "  ... showing {} of {} (--last N for more)",
                visible_archives.len(),
                archives.len()
            );
        }
        return 0;
    }

    // Selector provided — resolve archive
    let selector = args.selector.as_deref().unwrap();
    let resolved = match resolve_archive(selector, &archives) {
        Some(a) => a,
        None => {
            eprintln!("Error: Archive not found: {selector}");
            eprintln!("Run 'hcom archive' to list available archives");
            return 1;
        }
    };

    // Subcommand: agents
    if args.subcmd.as_deref() == Some("agents") {
        match query_archive_instances(resolved, sql_filter.as_deref()) {
            Ok(instances) => {
                if json_output {
                    println!("{}", serde_json::to_string(&instances).unwrap_or_default());
                } else if instances.is_empty() {
                    println!("Archive is empty");
                } else {
                    println!(
                        "{:<8} {:<8} {:<40} transcript",
                        "name", "status", "directory"
                    );
                    for inst in &instances {
                        let name = inst["name"].as_str().unwrap_or("?");
                        let status = inst["status"].as_str().unwrap_or("?");
                        let dir = shorten_path(inst["directory"].as_str().unwrap_or(""));
                        let transcript =
                            shorten_path(inst["transcript_path"].as_str().unwrap_or(""));
                        // Truncate for display
                        let name = &name[..name.len().min(8)];
                        let status = &status[..status.len().min(8)];
                        let dir = if dir.len() > 40 { &dir[..40] } else { &dir };
                        println!("{name:<8} {status:<8} {dir:<40} {transcript}");
                    }
                }
                0
            }
            Err(e) => {
                eprintln!("Error: Query failed: {e}");
                1
            }
        }
    } else {
        // Default: query events
        match query_archive_events(resolved, sql_filter.as_deref(), last_count.unwrap_or(20)) {
            Ok(events) => {
                if json_output {
                    println!("{}", serde_json::to_string(&events).unwrap_or_default());
                } else if events.is_empty() {
                    println!("No events in archive");
                } else {
                    for event in &events {
                        let eid = event["id"].as_i64().unwrap_or(0);
                        let ts_raw = event["timestamp"].as_str().unwrap_or("");
                        let ts = if let Some(t_idx) = ts_raw.find('T') {
                            &ts_raw[t_idx + 1..ts_raw.len().min(t_idx + 9)]
                        } else {
                            ts_raw
                        };
                        let etype = event["type"].as_str().unwrap_or("");
                        let inst = event["instance"].as_str().unwrap_or("");
                        let data = &event["data"];

                        match etype {
                            "message" => {
                                let text = data["text"].as_str().unwrap_or("");
                                let truncated = if text.len() > 60 {
                                    let end = (0..=60)
                                        .rev()
                                        .find(|&i| text.is_char_boundary(i))
                                        .unwrap_or(0);
                                    format!("{}...", &text[..end])
                                } else {
                                    text.to_string()
                                };
                                println!("#{eid} {ts} {etype:<8} {inst:<8} \"{truncated}\"");
                            }
                            "status" => {
                                let status = data["status"].as_str().unwrap_or("?");
                                let context = data["context"].as_str().unwrap_or("");
                                println!("#{eid} {ts} {etype:<8} {inst:<8} {status} {context}");
                            }
                            "life" => {
                                let action = data["action"].as_str().unwrap_or("?");
                                let by = data["by"].as_str().unwrap_or("");
                                if by.is_empty() {
                                    println!("#{eid} {ts} {etype:<8} {inst:<8} {action}");
                                } else {
                                    println!("#{eid} {ts} {etype:<8} {inst:<8} {action} by:{by}");
                                }
                            }
                            _ => {
                                println!("#{eid} {ts} {etype:<8} {inst:<8}");
                            }
                        }
                    }
                }
                0
            }
            Err(e) => {
                eprintln!("Error: Query failed: {e}");
                1
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_archive_args_last_flag() {
        use clap::Parser;
        let args = ArchiveArgs::try_parse_from(["archive", "--last", "50", "1"]).unwrap();
        assert_eq!(args.last, Some(50));
        assert_eq!(args.selector.as_deref(), Some("1"));
    }

    #[test]
    fn test_archive_args_last_default() {
        use clap::Parser;
        let args = ArchiveArgs::try_parse_from(["archive", "1"]).unwrap();
        assert_eq!(args.last, None);
        assert_eq!(args.selector.as_deref(), Some("1"));
    }

    #[test]
    fn test_archive_args_sql_flag() {
        use clap::Parser;
        let args =
            ArchiveArgs::try_parse_from(["archive", "--sql", "type='message'", "1"]).unwrap();
        assert_eq!(args.sql.as_deref(), Some("type='message'"));
        assert_eq!(args.selector.as_deref(), Some("1"));
    }

    #[test]
    fn test_archive_args_json_flag() {
        use clap::Parser;
        let args = ArchiveArgs::try_parse_from(["archive", "--json", "1"]).unwrap();
        assert!(args.json);
        assert_eq!(args.selector.as_deref(), Some("1"));
    }

    #[test]
    fn test_archive_list_limit() {
        let archives = vec![
            serde_json::json!({"index": 1}),
            serde_json::json!({"index": 2}),
            serde_json::json!({"index": 3}),
        ];
        assert_eq!(limit_archives(&archives, Some(2)).len(), 2);
        assert_eq!(limit_archives(&archives, Some(20)).len(), 3);
        assert!(limit_archives(&archives, Some(0)).is_empty());
        assert_eq!(limit_archives(&archives, None).len(), 3);
    }

    #[test]
    fn test_resolve_archive_by_index() {
        let archives = vec![
            serde_json::json!({"index": 1, "name": "session-2025-01-01_120000"}),
            serde_json::json!({"index": 2, "name": "session-2025-01-02_120000"}),
        ];
        let result = resolve_archive("1", &archives);
        assert!(result.is_some());
        assert_eq!(
            result.unwrap()["name"].as_str().unwrap(),
            "session-2025-01-01_120000"
        );
    }

    #[test]
    fn test_resolve_archive_by_name() {
        let archives = vec![serde_json::json!({"index": 1, "name": "session-2025-01-01_120000"})];
        let result = resolve_archive("2025-01-01", &archives);
        assert!(result.is_some());
    }

    #[test]
    fn test_resolve_archive_not_found() {
        let archives = vec![serde_json::json!({"index": 1, "name": "session-2025-01-01_120000"})];
        let result = resolve_archive("nonexistent", &archives);
        assert!(result.is_none());
    }

    #[test]
    fn test_shorten_path() {
        // Test with a path that doesn't start with HOME
        assert_eq!(shorten_path("/usr/local/bin"), "/usr/local/bin");
    }
}
