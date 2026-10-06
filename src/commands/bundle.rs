//! `hcom bundle` command — structured context sharing.
//!
//!
//! Subcommands: list, show, cat, chain, prepare/preview, create.

use std::path::Path;
use std::time::SystemTime;

use serde_json::{Value, json};

use crate::core::bundles;
use crate::core::filters::FILE_WRITE_CONTEXTS;
use crate::db::HcomDb;
use crate::shared::{CommandContext, SenderKind};

use crate::commands::transcript::{exact_instance_transcript, get_exchanges};
use crate::transcript::{Exchange, format_exchanges};

fn file_operations_query() -> String {
    format!(
        "SELECT id, timestamp, instance, data FROM events WHERE type = 'status' AND instance = ?1 AND (status_context IN {ctx} OR status_context LIKE 'tool:%' AND status_detail LIKE '/%') ORDER BY id DESC LIMIT ?2",
        ctx = FILE_WRITE_CONTEXTS
    )
}

/// Parsed arguments for `hcom bundle`.
///
/// Uses manual subcommand routing to support:
/// - `hcom bundle` → list (default)
/// - `hcom bundle --json --last 5` → list with flags
/// - `hcom bundle <id>` → implicit show
/// - `hcom bundle <subcmd> ...` → explicit subcommand
#[derive(clap::Parser, Debug)]
#[command(name = "bundle", about = "Manage context bundles")]
pub struct BundleArgs {
    /// All arguments (subcommand routing is manual due to implicit show/list)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

#[derive(clap::Parser, Debug)]
pub struct BundleListArgs {
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub last: Option<usize>,
}

#[derive(clap::Parser, Debug)]
pub struct BundleShowArgs {
    /// Bundle ID (event ID or bundle: prefix)
    pub id: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Parser, Debug)]
pub struct BundleCatArgs {
    /// Bundle ID
    pub id: String,
}

#[derive(clap::Parser, Debug)]
pub struct BundleChainArgs {
    /// Starting bundle ID
    pub id: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Parser, Debug)]
pub struct BundlePrepareArgs {
    #[arg(long)]
    pub json: bool,
    #[arg(long)]
    pub compact: bool,
    /// Target agent (default: self)
    #[arg(long = "for")]
    pub for_agent: Option<String>,
    /// Last N transcript exchanges (default: 40)
    #[arg(long = "last-transcript", default_value = "40")]
    pub last_transcript: usize,
    /// Last N events (default: 10)
    #[arg(long = "last-events", default_value = "10")]
    pub last_events: usize,
}

#[derive(clap::Parser, Debug)]
pub struct BundleCreateArgs {
    /// Bundle title (positional)
    pub title_positional: Option<String>,
    /// Bundle title (flag form, overrides positional)
    #[arg(long = "title")]
    pub title_flag: Option<String>,
    #[arg(long)]
    pub json: bool,
    /// Raw bundle JSON
    #[arg(long = "bundle")]
    pub bundle_json: Option<String>,
    /// Path to bundle JSON file
    #[arg(long = "bundle-file")]
    pub bundle_file: Option<String>,
    /// Bundle description
    #[arg(long)]
    pub description: Option<String>,
    /// Comma-separated event IDs
    #[arg(long)]
    pub events: Option<String>,
    /// Comma-separated file paths
    #[arg(long)]
    pub files: Option<String>,
    /// Transcript ranges (e.g., "3-14:normal,6:full")
    #[arg(long)]
    pub transcript: Option<String>,
    /// Parent bundle ID to extend
    #[arg(long)]
    pub extends: Option<String>,
}

// ── Bundle Lookup ────────────────────────────────────────────────────────

/// Find a bundle by ID (event ID or bundle_id prefix).
/// Returns bundle data with `event_id` and `timestamp` injected.
fn get_bundle_by_id(db: &HcomDb, id_or_prefix: &str) -> Option<Value> {
    // Try numeric event ID first
    if let Ok(event_id) = id_or_prefix.parse::<i64>()
        && let Ok(row) = db.conn().query_row(
            "SELECT id, timestamp, data FROM events WHERE id = ? AND type = 'bundle'",
            rusqlite::params![event_id],
            |row| {
                let id: i64 = row.get(0)?;
                let ts: String = row.get(1)?;
                let data_str: String = row.get(2)?;
                Ok((id, ts, data_str))
            },
        )
        && let Ok(mut data) = serde_json::from_str::<Value>(&row.2)
    {
        if let Some(obj) = data.as_object_mut() {
            obj.insert("event_id".into(), json!(row.0));
            obj.insert("timestamp".into(), json!(row.1));
        }
        return Some(data);
    }

    // Try bundle_id prefix match
    let bundle_id = if id_or_prefix.starts_with("bundle:") {
        id_or_prefix.to_string()
    } else {
        format!("bundle:{id_or_prefix}")
    };

    let query = "SELECT id, timestamp, data FROM events WHERE type = 'bundle' AND json_extract(data, '$.bundle_id') LIKE ? ORDER BY id DESC LIMIT 1";
    let pattern = format!("{bundle_id}%");

    db.conn()
        .query_row(query, rusqlite::params![pattern], |row| {
            let id: i64 = row.get(0)?;
            let ts: String = row.get(1)?;
            let data_str: String = row.get(2)?;
            Ok((id, ts, data_str))
        })
        .ok()
        .and_then(|(id, ts, data_str)| {
            serde_json::from_str::<Value>(&data_str)
                .ok()
                .map(|mut data| {
                    if let Some(obj) = data.as_object_mut() {
                        obj.insert("event_id".into(), json!(id));
                        obj.insert("timestamp".into(), json!(ts));
                    }
                    data
                })
        })
}

// ── Subcommands ──────────────────────────────────────────────────────────

/// List bundles: `hcom bundle [list] [--last N] [--json]`
fn cmd_bundle_list(db: &HcomDb, args: &BundleListArgs) -> i32 {
    let json_mode = args.json;
    let last_n = args.last.unwrap_or(20);

    let query = "SELECT id, timestamp, instance, data FROM events WHERE type = 'bundle' ORDER BY id DESC LIMIT ?";

    let mut stmt = match db.conn().prepare(query) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    let rows: Vec<(i64, String, String, String)> = stmt
        .query_map(rusqlite::params![last_n as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|r| r.ok())
        .collect();

    // JSON mode outputs even when empty (prints "[]")
    if json_mode {
        let mut bundles: Vec<Value> = Vec::new();
        for (id, ts, _inst, data_str) in &rows {
            let data: Value = serde_json::from_str(data_str).unwrap_or(json!({}));
            let events_val = data
                .get("refs")
                .and_then(|r| r.get("events"))
                .cloned()
                .unwrap_or(json!([]));
            bundles.push(json!({
                "id": id,
                "timestamp": ts,
                "bundle_id": data.get("bundle_id").and_then(|v| v.as_str()).unwrap_or(""),
                "title": data.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                "description": data.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                "created_by": data.get("created_by").and_then(|v| v.as_str()).unwrap_or(""),
                "events": events_val,
            }));
        }
        println!("{}", serde_json::to_string(&bundles).unwrap_or_default());
        return 0;
    }

    if rows.is_empty() {
        println!("No bundles found. Create one with: hcom bundle prepare");
        return 0;
    }

    // Human-readable table
    let now_secs = crate::shared::time::now_epoch_i64();

    println!("{:<12} {:<30} {:<12} AGE", "BUNDLE_ID", "TITLE", "BY");
    for (_id, ts, _inst, data_str) in &rows {
        let data: Value = serde_json::from_str(data_str).unwrap_or(json!({}));
        let bundle_id = data.get("bundle_id").and_then(|v| v.as_str()).unwrap_or("");
        let title = data
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("(untitled)");
        let created_by = data
            .get("created_by")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Parse timestamp for age
        let age = if let Ok(created) = chrono::DateTime::parse_from_rfc3339(ts)
            .or_else(|_| chrono::DateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S%.f%z"))
        {
            let age_secs = now_secs - created.timestamp();
            format_age(age_secs)
        } else {
            "?".to_string()
        };

        // Truncate display
        let short_id = if bundle_id.len() > 12 {
            &bundle_id[..12]
        } else {
            bundle_id
        };
        let short_title = if title.len() > 28 {
            let mut end = 25;
            while end > 0 && !title.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}...", &title[..end])
        } else {
            title.to_string()
        };

        println!("{short_id:<12} {short_title:<30} {created_by:<12} {age}");
    }

    0
}

/// Show bundle: `hcom bundle show <id> [--json]`
fn cmd_bundle_show(db: &HcomDb, args: &BundleShowArgs) -> i32 {
    let json_mode = args.json;

    let bundle = match get_bundle_by_id(db, &args.id) {
        Some(b) => b,
        None => {
            eprintln!("Error: Bundle not found: {}", args.id);
            return 1;
        }
    };

    if json_mode {
        println!("{}", serde_json::to_string(&bundle).unwrap_or_default());
    } else {
        let title = bundle.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let desc = bundle
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let bundle_id = bundle
            .get("bundle_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let created_by = bundle
            .get("created_by")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        println!("Bundle: {bundle_id}");
        println!("Title: {title}");
        println!("By: {created_by}");
        println!("Description: {desc}");

        if let Some(refs) = bundle.get("refs") {
            if let Some(events) = refs.get("events").and_then(|v| v.as_array()) {
                println!("\nEvents: {} referenced", events.len());
            }
            if let Some(files) = refs.get("files").and_then(|v| v.as_array()) {
                println!("Files: {} referenced", files.len());
            }
            if let Some(transcript) = refs.get("transcript").and_then(|v| v.as_array()) {
                println!("Transcript: {} ranges", transcript.len());
            }
        }

        if let Some(extends) = bundle.get("extends").and_then(|v| v.as_str()) {
            println!("\nExtends: {extends}");
        }
    }

    0
}

/// Cat bundle (expand full content): `hcom bundle cat <id>`
fn cmd_bundle_cat(db: &HcomDb, args: &BundleCatArgs) -> i32 {
    let bundle = match get_bundle_by_id(db, &args.id) {
        Some(b) => b,
        None => {
            eprintln!("Error: Bundle not found: {}", args.id);
            return 1;
        }
    };

    let title = bundle.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let desc = bundle
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let bundle_id = bundle
        .get("bundle_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let created_by = bundle
        .get("created_by")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    println!("=== Bundle: {bundle_id} ===");
    println!("Title: {title}");
    println!("By: {created_by}");
    println!("Description: {desc}");

    if let Some(refs) = bundle.get("refs") {
        // Files section (KB/MB size + "modified X ago")
        if let Some(files) = refs.get("files").and_then(|v| v.as_array()) {
            let sep = "━".repeat(80);
            println!("\n{sep}");
            println!("FILES ({})", files.len());
            println!("{sep}\n");
            for file_val in files {
                let path = file_val.as_str().unwrap_or("");
                if path.is_empty() {
                    continue;
                }
                let p = Path::new(path);
                if p.exists() {
                    let meta = std::fs::metadata(p).ok();
                    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                    let lines = std::fs::read_to_string(p)
                        .map(|c| c.lines().count())
                        .unwrap_or(0);
                    let size_str = if size < 1024 {
                        format!("{size} B")
                    } else if size < 1024 * 1024 {
                        format!("{:.1} KB", size as f64 / 1024.0)
                    } else {
                        format!("{:.1} MB", size as f64 / (1024.0 * 1024.0))
                    };
                    let modified_str = meta
                        .as_ref()
                        .and_then(|m| m.modified().ok())
                        .and_then(|mt| SystemTime::now().duration_since(mt).ok())
                        .map(|d| {
                            let secs = d.as_secs() as i64;
                            if secs < 3600 {
                                format!("{}m ago", secs / 60)
                            } else if secs < 86400 {
                                format!("{}h ago", secs / 3600)
                            } else {
                                format!("{}d ago", secs / 86400)
                            }
                        })
                        .unwrap_or_default();
                    println!("{path} ({lines} lines, {size_str}, modified {modified_str})");
                } else {
                    println!("{path} (not found)");
                }
            }
            println!();
        }

        // Events section — full JSON output, supports ID ranges like "100-105"
        if let Some(events) = refs.get("events").and_then(|v| v.as_array()) {
            let sep = "━".repeat(80);
            println!("\n{sep}");
            println!("EVENTS ({})", events.len());
            println!("{sep}\n");

            // Parse event refs: individual IDs or ranges like "100-105"
            let mut event_ids: Vec<i64> = Vec::new();
            for event_ref in events {
                let ref_str = event_ref
                    .as_str()
                    .map(|s| s.to_string())
                    .or_else(|| event_ref.as_i64().map(|n| n.to_string()))
                    .unwrap_or_default();
                if ref_str.contains('-') {
                    let parts: Vec<&str> = ref_str.splitn(2, '-').collect();
                    if let (Ok(start), Ok(end)) = (
                        parts[0].parse::<i64>(),
                        parts.get(1).unwrap_or(&"").parse::<i64>(),
                    ) {
                        event_ids.extend(start..=end);
                    }
                } else if let Ok(id) = ref_str.parse::<i64>() {
                    event_ids.push(id);
                }
            }

            if !event_ids.is_empty() {
                let placeholders = event_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                let query = format!(
                    "SELECT id, timestamp, type, instance, data FROM events WHERE id IN ({}) ORDER BY id ASC",
                    placeholders
                );
                if let Ok(mut stmt) = db.conn().prepare(&query) {
                    let params: Vec<Box<dyn rusqlite::ToSql>> = event_ids
                        .iter()
                        .map(|id| Box::new(*id) as Box<dyn rusqlite::ToSql>)
                        .collect();
                    let param_refs: Vec<&dyn rusqlite::ToSql> =
                        params.iter().map(|p| p.as_ref()).collect();
                    if let Ok(rows) = stmt.query_map(param_refs.as_slice(), |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    }) {
                        for row in rows.flatten() {
                            let (id, ts, etype, inst, data_str) = row;
                            let data: Value = serde_json::from_str(&data_str).unwrap_or(json!({}));
                            let event_obj = json!({
                                "id": id,
                                "timestamp": ts,
                                "type": etype,
                                "instance": inst,
                                "data": data,
                            });
                            println!(
                                "{}",
                                serde_json::to_string_pretty(&event_obj).unwrap_or_default()
                            );
                            println!();
                        }
                    }
                }
            }
        }

        if let Some(transcript) = refs.get("transcript").and_then(|v| v.as_array()) {
            let sep = "━".repeat(80);
            println!("\n{sep}");
            println!("TRANSCRIPT ({} entries)", transcript.len());
            println!("{sep}\n");
            print_bundle_transcript(db, created_by, transcript);
        }
    }

    0
}

/// Render each `range:detail` transcript ref against the creator's transcript.
fn print_bundle_transcript(db: &HcomDb, created_by: &str, refs: &[Value]) {
    let Some((path, tool, session_id)) = exact_instance_transcript(db, created_by) else {
        println!("Transcript unavailable: agent '{created_by}' not found or has no transcript");
        return;
    };
    if !Path::new(&path).exists() {
        println!("Transcript unavailable: {path} not found");
        return;
    }

    // Parsed once per detail mode: [normal/full, detailed].
    let mut cache: [Option<Result<Vec<Exchange>, String>>; 2] = [None, None];
    for tref in refs {
        let ref_data = match bundles::parse_transcript_ref(tref) {
            Ok(r) => r,
            Err(e) => {
                println!("  (invalid ref: {e})");
                continue;
            }
        };
        let range = ref_data.get("range").and_then(|v| v.as_str()).unwrap_or("");
        let detail = ref_data
            .get("detail")
            .and_then(|v| v.as_str())
            .unwrap_or("normal");
        let full = detail == "full" || detail == "detailed";
        let detailed = detail == "detailed";

        println!("--- Transcript [{range}] ({detail}) ---\n");

        let Some((start, end)) = parse_exchange_range(range) else {
            println!("  (invalid range: {range})\n");
            continue;
        };
        let exchanges = cache[detailed as usize].get_or_insert_with(|| {
            get_exchanges(
                &path,
                &tool,
                usize::MAX,
                detailed,
                session_id.as_deref(),
                true,
            )
        });
        match exchanges {
            Ok(all) => {
                let selected: Vec<Exchange> = all
                    .iter()
                    .filter(|e| e.position >= start && e.position <= end)
                    .cloned()
                    .collect();
                if selected.is_empty() {
                    println!(
                        "  (no exchanges in range; transcript has {} exchanges)\n",
                        all.len()
                    );
                } else {
                    println!("{}", format_exchanges(&selected, full, detailed));
                }
            }
            Err(e) => println!("Error reading transcript: {e}\n"),
        }
    }
}

/// Parse `N` or `N-M` (1-based, inclusive).
fn parse_exchange_range(range: &str) -> Option<(usize, usize)> {
    let (start, end) = match range.split_once('-') {
        Some((s, e)) => (s.trim().parse().ok()?, e.trim().parse().ok()?),
        None => {
            let pos = range.trim().parse().ok()?;
            (pos, pos)
        }
    };
    (start >= 1 && start <= end).then_some((start, end))
}

/// Chain: `hcom bundle chain <id> [--json]`
fn cmd_bundle_chain(db: &HcomDb, args: &BundleChainArgs) -> i32 {
    let json_mode = args.json;

    let mut chain = Vec::new();
    let mut current_id = args.id.clone();
    let mut seen = std::collections::HashSet::new();

    // First bundle must exist
    let first = match get_bundle_by_id(db, &current_id) {
        Some(b) => b,
        None => {
            eprintln!("Error: Bundle not found: {}", args.id);
            return 1;
        }
    };

    let bid = first
        .get("bundle_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    seen.insert(bid.clone());
    chain.push(first.clone());

    // Walk up the chain
    if let Some(parent_id) = first.get("extends").and_then(|v| v.as_str()) {
        current_id = parent_id.to_string();
        loop {
            let bundle = match get_bundle_by_id(db, &current_id) {
                Some(b) => b,
                None => {
                    // Warn about missing ancestor
                    eprintln!("Warning: missing ancestor bundle {current_id}");
                    break;
                }
            };

            let bid = bundle
                .get("bundle_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if seen.contains(&bid) {
                break; // Cycle detection
            }
            seen.insert(bid.clone());
            chain.push(bundle.clone());

            match bundle.get("extends").and_then(|v| v.as_str()) {
                Some(parent) => current_id = parent.to_string(),
                None => break,
            }
        }
    }

    if json_mode {
        println!("{}", serde_json::to_string(&chain).unwrap_or_default());
        return 0;
    }

    println!("Bundle chain ({} levels):", chain.len());
    for (i, bundle) in chain.iter().enumerate() {
        let bid = bundle
            .get("bundle_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let title = bundle
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("(untitled)");
        let indent = "  ".repeat(i);
        let marker = if i == 0 { "→" } else { "↳" };
        println!("{indent}{marker} {bid}: {title}");
    }

    0
}

/// One prepare event category: (display label, JSON key, rows `(id, timestamp, instance, data)`).
type EventCategory = (
    &'static str,
    &'static str,
    Vec<(i64, String, String, Value)>,
);

/// Recent events touching `agent`, newest first, `limit` per category.
fn bundle_event_categories(db: &HcomDb, agent: &str, limit: usize) -> Vec<EventCategory> {
    let queries: [(&str, &str, String); 4] = [
        (
            "Messages to",
            "messages_to",
            "SELECT id, timestamp, instance, data FROM events WHERE type = 'message' AND EXISTS (SELECT 1 FROM json_each(json_extract(data, '$.delivered_to')) WHERE value = ?1) ORDER BY id DESC LIMIT ?2".to_string(),
        ),
        (
            "Messages from",
            "messages_from",
            "SELECT id, timestamp, instance, data FROM events WHERE type = 'message' AND json_extract(data, '$.from') = ?1 ORDER BY id DESC LIMIT ?2".to_string(),
        ),
        ("File operations", "file_operations", file_operations_query()),
        (
            "Lifecycle",
            "lifecycle",
            "SELECT id, timestamp, instance, data FROM events WHERE type = 'life' AND (instance = ?1 OR json_extract(data, '$.by') = ?1) ORDER BY id DESC LIMIT ?2".to_string(),
        ),
    ];

    queries
        .into_iter()
        .map(|(label, key, query)| {
            let rows = db
                .conn()
                .prepare(&query)
                .and_then(|mut stmt| {
                    stmt.query_map(rusqlite::params![agent, limit as i64], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()
                })
                .unwrap_or_default()
                .into_iter()
                .map(|(id, ts, inst, data)| {
                    (
                        id,
                        ts,
                        inst,
                        serde_json::from_str(&data).unwrap_or(json!({})),
                    )
                })
                .collect();
            (label, key, rows)
        })
        .collect()
}

/// Prepare: `hcom bundle prepare [--for AGENT] [--last-transcript N] [--last-events N] [--compact] [--json]`
fn cmd_bundle_prepare(db: &HcomDb, args: &BundlePrepareArgs, ctx: Option<&CommandContext>) -> i32 {
    let last_transcript = args.last_transcript;
    let last_events = args.last_events;

    // The caller creates the bundle, whichever agent's context it covers.
    let caller = ctx
        .and_then(|c| c.identity.as_ref())
        .filter(|id| matches!(id.kind, SenderKind::Instance))
        .map(|id| id.name.clone());
    let agent = match args
        .for_agent
        .as_deref()
        .map(|name| {
            crate::identity::resolve_display_name(db, name).unwrap_or_else(|| name.to_string())
        })
        .or_else(|| caller.clone())
    {
        Some(name) => name,
        None => {
            eprintln!("Error: No agent context. Use --for <agent> to specify.");
            return 1;
        }
    };

    let mut transcript_text: Option<String> = None;
    let mut transcript_range: Option<String> = None;
    if let Some((path, tool, session_id)) = exact_instance_transcript(db, &agent) {
        match get_exchanges(
            &path,
            &tool,
            last_transcript,
            false,
            session_id.as_deref(),
            true,
        ) {
            Ok(exchanges) => {
                if let (Some(first), Some(last)) = (exchanges.first(), exchanges.last()) {
                    transcript_range = Some(if first.position == last.position {
                        first.position.to_string()
                    } else {
                        format!("{}-{}", first.position, last.position)
                    });
                    transcript_text = Some(format_exchanges(&exchanges, false, false));
                }
            }
            Err(e) => transcript_text = Some(format!("Error reading transcript: {e}")),
        }
    }

    let categories = bundle_event_categories(db, &agent, last_events);

    let mut event_ids: Vec<i64> = categories
        .iter()
        .flat_map(|(_, _, rows)| rows.iter().map(|(id, ..)| *id))
        .collect();
    event_ids.sort_unstable_by(|a, b| b.cmp(a));
    event_ids.dedup();

    let mut files: Vec<String> = categories
        .iter()
        .flat_map(|(_, _, rows)| rows.iter())
        .filter_map(|(.., data)| data.get("detail").and_then(|v| v.as_str()))
        .filter(|detail| {
            detail.contains('/')
                || [".py", ".ts", ".js", ".md", ".json", ".rs"]
                    .iter()
                    .any(|ext| detail.ends_with(ext))
        })
        .map(str::to_string)
        .collect();
    files.sort();
    files.dedup();

    // Template command (events capped at 20, files at 10)
    let mut template_parts = vec![match caller {
        Some(ref name) => format!("hcom bundle create \"Bundle Title Here\" --name {name}"),
        None => "hcom bundle create \"Bundle Title Here\"".to_string(),
    }];
    template_parts.push("--description \"detailed description text here\"".to_string());
    if let Some(ref range) = transcript_range {
        template_parts.push(format!("--transcript \"{range}:normal\""));
    }
    if !event_ids.is_empty() {
        let ids: Vec<String> = event_ids.iter().take(20).map(i64::to_string).collect();
        template_parts.push(format!("--events \"{}\"", ids.join(",")));
    }
    if !files.is_empty() {
        let sample: Vec<&str> = files.iter().take(10).map(String::as_str).collect();
        template_parts.push(format!("--files \"{}\"", sample.join(",")));
    }
    let template_command = template_parts.join(" \\\n  ");

    if args.json {
        let events_json: serde_json::Map<String, Value> = categories
            .iter()
            .map(|(_, key, rows)| {
                let events: Vec<Value> = rows
                    .iter()
                    .map(|(id, ts, inst, data)| {
                        json!({"id": id, "timestamp": ts, "instance": inst, "data": data})
                    })
                    .collect();
                (key.to_string(), json!(events))
            })
            .collect();
        let result = json!({
            "agent": agent,
            "transcript": {
                "text": transcript_text,
                "range": transcript_range,
            },
            "events": events_json,
            "files": files,
            "template_command": template_command,
            "note": format!("Last {last_transcript} transcript exchanges, {last_events} events per category"),
        });
        println!("{}", serde_json::to_string(&result).unwrap_or_default());
        return 0;
    }

    let sep = "─".repeat(40);
    println!("[Bundle Context: {agent}]\n");

    if !args.compact {
        println!("{sep}");
        println!("HOW TO USE THIS CONTEXT:\n");
        println!(
            "Edit the CREATE command below into your bundle, or pass the same flags to \
'hcom send' (with --title) to create and send in one step."
        );
        println!(
            "Transcript refs: range:detail, comma-separated (e.g. 3-14:normal,6:full,22-30:detailed)"
        );
        println!(
            "  normal = truncated | full = complete text | detailed = full + tool I/O, edits, errors\n"
        );
        println!("- Pick the relevant transcript ranges, events, and files from the context below");
        println!("- Use hcom transcript and hcom events to find anything else relevant");
        println!(
            "- Give each transcript range the detail it needs \
(full when the whole text matters, normal only when the summary below is enough)"
        );
        println!(
            "- Description: be comprehensive and precise. Explain what the bundle contains, \
summarise the referenced transcript ranges and events, and give enough insight for another agent \
to understand everything you know: what happened, decisions, current state, issues, plans.\n"
        );
        println!("A good bundle includes everything relevant and nothing irrelevant.\n");
        println!("View: hcom transcript {agent} [N-M] [--full|--detailed]");
        println!("View: hcom events --agent {agent} [--last N]\n");
        println!("Use hcom bundle prepare --compact to hide this section\n");
    }

    println!("{sep}");
    println!("TRANSCRIPT");
    println!("{}\n", transcript_text.as_deref().unwrap_or("(none)"));

    for (label, _, rows) in &categories {
        if rows.is_empty() {
            continue;
        }
        println!("--- {label} ({} events) ---", rows.len());
        for (id, ts, inst, data) in rows {
            let ts_short = ts.get(..19).unwrap_or(ts);
            println!(
                "  #{id} | {inst} @ {ts_short} | {}",
                format_event_summary(data)
            );
        }
        println!();
    }

    if !files.is_empty() {
        println!("{sep}");
        println!("FILES ({})", files.len());
        for f in files.iter().take(20) {
            // Show relative-ish path (last 3 components)
            let parts: Vec<&str> = f.split('/').collect();
            let short = if parts.len() > 3 {
                parts[parts.len() - 3..].join("/")
            } else {
                f.clone()
            };
            println!("  {short}");
        }
        if files.len() > 20 {
            println!("  +{} more", files.len() - 20);
        }
        println!();
    }

    println!("{sep}");
    println!("CREATE:");
    println!("{template_command}");

    0
}

/// Create: `hcom bundle create [TITLE] --description DESC [--events LIST] [--files LIST] [--transcript RANGES] [--extends ID] [--json]`
fn cmd_bundle_create(db: &HcomDb, args: &BundleCreateArgs, ctx: Option<&CommandContext>) -> i32 {
    let json_mode = args.json;

    // Mutual exclusion: --bundle and --bundle-file
    if args.bundle_json.is_some() && args.bundle_file.is_some() {
        eprintln!("Error: --bundle and --bundle-file are mutually exclusive");
        return 1;
    }

    // Raw bundle mode
    if let Some(raw) = args.bundle_json.as_ref().cloned().or_else(|| {
        args.bundle_file
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
    }) {
        let mut bundle: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Error: Invalid JSON: {e}");
                return 1;
            }
        };
        return create_and_log_bundle(db, &mut bundle, ctx, json_mode);
    }

    // Inline bundle creation mode
    let title = match args.title_flag.as_ref().or(args.title_positional.as_ref()) {
        Some(t) => t.clone(),
        None => {
            eprintln!(
                "Usage: hcom bundle create TITLE --description DESC [--events LIST] [--files LIST] [--transcript RANGES]"
            );
            return 1;
        }
    };

    let description = match &args.description {
        Some(d) => d.clone(),
        None => {
            eprintln!("Error: --description is required");
            return 1;
        }
    };

    // Build bundle data
    let events_list = bundles::parse_csv_list(args.events.as_deref());
    let files_list = bundles::parse_csv_list(args.files.as_deref());

    let transcript_refs: Vec<Value> = if let Some(ref t) = args.transcript {
        t.split(',')
            .filter_map(|s| {
                let s = s.trim();
                if s.is_empty() {
                    return None;
                }
                bundles::parse_transcript_ref(&json!(s)).ok()
            })
            .collect()
    } else {
        vec![]
    };

    let mut bundle = json!({
        "title": title,
        "description": description,
        "refs": {
            "events": events_list.iter().map(|e| json!(e)).collect::<Vec<_>>(),
            "files": files_list.iter().map(|f| json!(f)).collect::<Vec<_>>(),
            "transcript": transcript_refs,
        },
    });

    if let Some(ref ext) = args.extends {
        bundle["extends"] = json!(ext);
    }

    create_and_log_bundle(db, &mut bundle, ctx, json_mode)
}

/// Validate, create, and log a bundle event.
fn create_and_log_bundle(
    db: &HcomDb,
    bundle: &mut Value,
    ctx: Option<&CommandContext>,
    json_mode: bool,
) -> i32 {
    // Resolve instance name
    let identity = ctx.and_then(|c| c.identity.as_ref());
    let instance = identity
        .map(|id| match id.kind {
            SenderKind::External => format!("ext_{}", id.name),
            SenderKind::System => format!("sys_{}", id.name),
            SenderKind::Instance => id.name.clone(),
        })
        .unwrap_or_else(|| "cli".to_string());
    let created_by = identity.map(|id| id.name.as_str());

    // Create bundle event
    match bundles::create_bundle_event(bundle, &instance, created_by, db) {
        Ok(bundle_id) => {
            // Trigger relay push (best-effort)
            crate::relay::spawn_background_push();

            if json_mode {
                println!("{}", json!({"bundle_id": bundle_id}));
            } else {
                // Print raw bundle_id for scripting compatibility
                println!("{bundle_id}");
            }
            0
        }
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

/// Format a short summary for an event data object.
fn format_event_summary(data: &Value) -> String {
    if let Some(text) = data.get("text").and_then(|v| v.as_str()) {
        let short = if text.len() > 50 {
            let mut end = 47;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}...", &text[..end])
        } else {
            text.to_string()
        };
        return format!("\"{}\"", short.replace('\n', " "));
    }
    if let Some(action) = data.get("action").and_then(|v| v.as_str()) {
        return action.to_string();
    }
    if let Some(status) = data.get("status").and_then(|v| v.as_str()) {
        let ctx = data.get("context").and_then(|v| v.as_str()).unwrap_or("");
        return if ctx.is_empty() {
            status.to_string()
        } else {
            format!("{status}:{ctx}")
        };
    }
    "(no summary)".to_string()
}

use crate::shared::time::format_age;

// ── Main Entry Point ─────────────────────────────────────────────────────

/// Main entry point for `hcom bundle` command.
/// Manual subcommand routing to support implicit list/show patterns.
pub fn cmd_bundle(db: &HcomDb, args: &BundleArgs, ctx: Option<&CommandContext>) -> i32 {
    let argv = &args.args;
    let subcmd = argv.first().map(|s| s.as_str()).unwrap_or("list");
    let sub_argv: Vec<String> = if argv.is_empty() {
        vec![]
    } else {
        argv[1..].to_vec()
    };

    /// Try parsing a clap Args struct from sub-argv. Returns exit code on error.
    fn try_parse<T: clap::Parser>(name: &str, argv: &[String]) -> Result<T, i32> {
        T::try_parse_from(std::iter::once(name.to_string()).chain(argv.iter().cloned())).map_err(
            |e| {
                e.print().ok();
                if e.use_stderr() { 1 } else { 0 }
            },
        )
    }

    match subcmd {
        "list" => match try_parse::<BundleListArgs>("bundle list", &sub_argv) {
            Ok(a) => cmd_bundle_list(db, &a),
            Err(code) => code,
        },
        "show" => match try_parse::<BundleShowArgs>("bundle show", &sub_argv) {
            Ok(a) => cmd_bundle_show(db, &a),
            Err(code) => code,
        },
        "cat" => match try_parse::<BundleCatArgs>("bundle cat", &sub_argv) {
            Ok(a) => cmd_bundle_cat(db, &a),
            Err(code) => code,
        },
        "chain" => match try_parse::<BundleChainArgs>("bundle chain", &sub_argv) {
            Ok(a) => cmd_bundle_chain(db, &a),
            Err(code) => code,
        },
        "prepare" | "preview" => {
            match try_parse::<BundlePrepareArgs>("bundle prepare", &sub_argv) {
                Ok(a) => cmd_bundle_prepare(db, &a, ctx),
                Err(code) => code,
            }
        }
        "create" => match try_parse::<BundleCreateArgs>("bundle create", &sub_argv) {
            Ok(a) => cmd_bundle_create(db, &a, ctx),
            Err(code) => code,
        },
        _ => {
            if subcmd.starts_with('-') {
                // Flags without subcommand → list mode: `hcom bundle --json --last 5`
                match try_parse::<BundleListArgs>("bundle list", argv) {
                    Ok(a) => cmd_bundle_list(db, &a),
                    Err(code) => code,
                }
            } else {
                // Any non-flag, non-subcommand token → implicit show (bundle ID or prefix)
                // get_bundle_by_id() handles numeric IDs, bundle: prefix, and bare prefix lookup
                match try_parse::<BundleShowArgs>("bundle show", argv) {
                    Ok(a) => cmd_bundle_show(db, &a),
                    Err(code) => code,
                }
            }
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::time::now_epoch_f64;
    use std::fs;

    #[test]
    fn test_format_age() {
        assert_eq!(format_age(30), "30s");
        assert_eq!(format_age(90), "1m");
        assert_eq!(format_age(3700), "1h");
        assert_eq!(format_age(90000), "1d");
    }

    #[test]
    fn test_format_event_summary_message() {
        let data = json!({"text": "hello world"});
        let summary = format_event_summary(&data);
        assert!(summary.contains("hello world"));
    }

    #[test]
    fn test_format_event_summary_status() {
        let data = json!({"status": "active", "context": "tool:Write"});
        let summary = format_event_summary(&data);
        assert_eq!(summary, "active:tool:Write");
    }

    #[test]
    fn test_format_event_summary_life() {
        let data = json!({"action": "stopped"});
        let summary = format_event_summary(&data);
        assert_eq!(summary, "stopped");
    }

    #[test]
    fn test_format_event_summary_truncation() {
        let long_text = "x".repeat(100);
        let data = json!({"text": long_text});
        let summary = format_event_summary(&data);
        assert!(summary.len() < 60);
        assert!(summary.ends_with("...\""));
    }

    // ── Clap sub-struct parse tests ────────────────────────────────

    use clap::Parser;

    #[test]
    fn test_bundle_top_level_passthrough() {
        // Top-level BundleArgs uses trailing_var_arg — accepts anything
        let args = BundleArgs::try_parse_from(["bundle"]).unwrap();
        assert!(args.args.is_empty());

        let args = BundleArgs::try_parse_from(["bundle", "--json", "--last", "5"]).unwrap();
        assert_eq!(args.args, vec!["--json", "--last", "5"]);

        let args = BundleArgs::try_parse_from(["bundle", "bundle:abc123"]).unwrap();
        assert_eq!(args.args, vec!["bundle:abc123"]);
    }

    #[test]
    fn test_bundle_list_parse() {
        let a = BundleListArgs::try_parse_from(["list", "--json", "--last", "5"]).unwrap();
        assert!(a.json);
        assert_eq!(a.last, Some(5));
    }

    #[test]
    fn test_bundle_show_parse() {
        let a = BundleShowArgs::try_parse_from(["show", "bundle:abc123"]).unwrap();
        assert_eq!(a.id, "bundle:abc123");
    }

    #[test]
    fn test_bundle_prepare_parse() {
        let a = BundlePrepareArgs::try_parse_from([
            "prepare",
            "--for",
            "peso",
            "--compact",
            "--last-transcript",
            "10",
        ])
        .unwrap();
        assert_eq!(a.for_agent.as_deref(), Some("peso"));
        assert!(a.compact);
        assert_eq!(a.last_transcript, 10);
    }

    #[test]
    fn test_bundle_create_positional_title() {
        let a =
            BundleCreateArgs::try_parse_from(["create", "My Bundle", "--description", "A test"])
                .unwrap();
        assert_eq!(a.title_positional.as_deref(), Some("My Bundle"));
        assert_eq!(a.description.as_deref(), Some("A test"));
    }

    #[test]
    fn test_bundle_create_flag_title() {
        let a = BundleCreateArgs::try_parse_from([
            "create",
            "--title",
            "My Bundle",
            "--description",
            "A test",
        ])
        .unwrap();
        assert_eq!(a.title_flag.as_deref(), Some("My Bundle"));
        assert_eq!(a.description.as_deref(), Some("A test"));
    }

    #[test]
    fn test_bundle_list_rejects_bogus() {
        assert!(BundleListArgs::try_parse_from(["list", "--bogus"]).is_err());
    }

    fn test_db() -> HcomDb {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();
        std::mem::forget(dir);
        db
    }

    #[test]
    fn test_bundle_transcript_source_falls_back_to_stopped_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let transcript_path = dir.path().join("claude.jsonl");
        let db = test_db();

        let lines = [
            json!({
                "type": "user",
                "timestamp": "2026-04-13T12:00:00Z",
                "message": {
                    "content": [{"type": "text", "text": "remember marker"}]
                }
            }),
            json!({
                "type": "assistant",
                "timestamp": "2026-04-13T12:00:01Z",
                "message": {
                    "content": [{"type": "text", "text": "ack marker"}]
                }
            }),
        ];
        fs::write(
            &transcript_path,
            lines
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let snapshot = json!({
            "name": "huno",
            "tool": "claude",
            "session_id": "sess-123",
            "transcript_path": transcript_path.to_string_lossy().to_string(),
            "created_at": now_epoch_f64(),
        });
        db.log_life_event("huno", "stopped", "cli", "killed", Some(snapshot))
            .unwrap();

        let (path, tool, sid) = exact_instance_transcript(&db, "huno").unwrap();
        assert_eq!(path, transcript_path.to_string_lossy());
        assert_eq!(tool, "claude");
        assert_eq!(sid.as_deref(), Some("sess-123"));

        let exchanges = get_exchanges(&path, &tool, 10, false, sid.as_deref(), true).unwrap();
        assert_eq!(exchanges.len(), 1);
        assert_eq!(exchanges[0].position, 1);
    }

    #[test]
    fn test_bundle_transcript_source_does_not_invent_claude_metadata() {
        let db = test_db();
        assert_eq!(exact_instance_transcript(&db, "missing"), None);
    }

    #[test]
    fn test_parse_exchange_range() {
        assert_eq!(parse_exchange_range("7"), Some((7, 7)));
        assert_eq!(parse_exchange_range("3-14"), Some((3, 14)));
        assert_eq!(parse_exchange_range("0"), None);
        assert_eq!(parse_exchange_range("9-3"), None);
        assert_eq!(parse_exchange_range("x-3"), None);
    }
}
