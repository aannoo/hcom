//! `hcom list` command — list active instances.
//!
//!
//! Supports: human-readable, --json, --names, --format, -v,
//! single instance query (self/named), full listing with unread counts.

use std::collections::HashMap;

use crate::db::{HcomDb, InstanceRow};
use crate::identity;
use crate::identity::{get_full_name, resolve_display_name};
use crate::instance_lifecycle::{
    RECENTLY_STOPPED_WINDOW, cleanup_stale_instances, cleanup_stale_placeholders, format_age,
    get_instance_status,
};
use crate::instances::is_remote_instance;
use crate::shared::{CommandContext, SENDER, ST_LISTENING, shorten_path_max, status_icon};

/// Parsed arguments for `hcom list`.
#[derive(clap::Parser, Debug)]
#[command(name = "list", about = "List active agents")]
pub struct ListArgs {
    /// Agent name or "self"
    pub name: Option<String>,
    /// Field to extract (used with name)
    pub field: Option<String>,
    /// Show recently stopped agents
    #[arg(long)]
    pub stopped: bool,
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Verbose output
    #[arg(short = 'v', long)]
    pub verbose: bool,
    /// Names-only output
    #[arg(long)]
    pub names: bool,
    /// Shell export format
    #[arg(long)]
    pub sh: bool,
    /// Custom format template with {field} placeholders
    #[arg(long)]
    pub format: Option<String>,
    /// Show all (with --stopped)
    #[arg(long)]
    pub all: bool,
    /// Limit results (with --stopped)
    #[arg(long)]
    pub last: Option<usize>,
    /// Filter by project (show only agents in this project)
    #[arg(long)]
    pub project: Option<String>,
}

/// Get unread message count for a single instance.
fn get_unread_count(db: &HcomDb, name: &str, last_event_id: i64) -> i64 {
    db.conn()
        .query_row(
            "SELECT COUNT(*) FROM events WHERE id > ? AND type = 'message'
             AND EXISTS (SELECT 1 FROM json_each(json_extract(data, '$.delivered_to')) WHERE value = ?)",
            rusqlite::params![last_event_id, name],
            |row| row.get(0),
        )
        .unwrap_or(0)
}

/// Get unread counts for all instances in batch.
fn get_unread_counts_batch(db: &HcomDb, instances: &[InstanceRow]) -> HashMap<String, i64> {
    let mut counts = HashMap::new();
    for inst in instances {
        if is_remote_instance(inst) {
            continue;
        }
        let count = get_unread_count(db, &inst.name, inst.last_event_id);
        if count > 0 {
            counts.insert(inst.name.clone(), count);
        }
    }
    counts
}

/// Main entry point for `hcom list` command.
///
/// Returns exit code (0 = success, 1 = error).
pub fn cmd_list(db: &HcomDb, args: &ListArgs, ctx: Option<&CommandContext>) -> i32 {
    // Clean up stale placeholders and instances
    cleanup_stale_placeholders(db);
    let _ = cleanup_stale_instances(db, 3600, 3600);

    let explicit_name = ctx.and_then(|c| c.explicit_name.as_deref());

    // --stopped: show recently stopped instances from life events
    if args.stopped {
        return cmd_list_stopped(db, args);
    }

    let json_output = args.json;
    let verbose_output = args.verbose;
    let names_output = args.names;
    let sh_output = args.sh;
    let format_template = args.format.clone();
    let target_name = args.name.as_deref();
    let field_name = args.field.as_deref();

    // Resolve current instance identity
    let (sender_identity, current_name) = if let Some(id) = ctx.and_then(|c| c.identity.as_ref()) {
        (Some(id.clone()), Some(id.name.clone()))
    } else if let Some(name) = explicit_name {
        match identity::resolve_identity(db, Some(name), None, None, None, None, None) {
            Ok(id) => {
                let n = id.name.clone();
                (Some(id), Some(n))
            }
            Err(e) => {
                eprintln!("Error: Cannot resolve '{name}': {e}");
                return 1;
            }
        }
    } else {
        identity::resolve_identity(db, None, None, None, None, None, None)
            .map(|id| {
                let n = id.name.clone();
                (Some(id), Some(n))
            })
            .unwrap_or((None, None))
    };

    // Single instance query: hcom list <name|self> [field] [--json]
    if let Some(target) = target_name {
        let is_self = target == "self";

        if is_self && sender_identity.is_none() {
            eprintln!("Error: Cannot use 'self' without identity. Run 'hcom start' first.");
            return 1;
        }

        if is_self {
            let name = current_name.as_deref().unwrap_or("");
            let mut payload = serde_json::json!({
                "name": name,
                "session_id": sender_identity.as_ref().and_then(|id| id.session_id.as_deref()).unwrap_or(""),
            });

            if !name.is_empty() && name != SENDER {
                if let Ok(Some(data)) = db.get_instance_full(name) {
                    payload["status"] = serde_json::json!(data.status);
                    payload["transcript_path"] = serde_json::json!(data.transcript_path);
                    payload["directory"] = serde_json::json!(data.directory);
                    payload["parent_name"] = serde_json::json!(data.parent_name);
                    payload["agent_id"] = serde_json::json!(data.agent_id);
                    payload["tool"] = serde_json::json!(data.tool);
                }
            }

            if let Some(field) = field_name {
                println!("{}", extract_field_value(&payload, field));
            } else if sh_output {
                print_sh_exports(&payload);
            } else if json_output {
                println!("{}", serde_json::to_string(&payload).unwrap_or_default());
            } else {
                println!("{name}");
            }
            return 0;
        }

        // Named instance query
        let resolved = resolve_display_name(db, target);
        let lookup_name = resolved.as_deref().unwrap_or(target);

        match db.get_instance_full(lookup_name) {
            Ok(Some(data)) => {
                let payload = serde_json::json!({
                    "name": lookup_name,
                    "session_id": data.session_id,
                    "status": data.status,
                    "directory": data.directory,
                    "transcript_path": data.transcript_path,
                    "parent_name": data.parent_name,
                    "agent_id": data.agent_id,
                    "tool": data.tool,
                });

                if let Some(field) = field_name {
                    println!("{}", extract_field_value(&payload, field));
                } else if sh_output {
                    print_sh_exports(&payload);
                } else if json_output {
                    println!("{}", serde_json::to_string(&payload).unwrap_or_default());
                } else {
                    println!("{lookup_name}:");
                    println!("  Status: {}", data.status);
                    println!("  Directory: {}", data.directory);
                    if let Some(ref sid) = data.session_id {
                        println!("  Session: {sid}");
                    }
                }
            }
            _ => {
                eprintln!("Error: Not found: {target}");
                eprintln!("Use 'hcom list' to see active agents.");
                return 1;
            }
        }
        return 0;
    }

    // Full listing mode
    let sorted_instances = match db.iter_instances_full() {
        Ok(rows) => rows,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    // Filter by project if --project provided
    let sorted_instances: Vec<InstanceRow> = if let Some(ref proj) = args.project {
        let proj = proj.trim();
        if proj.is_empty() {
            sorted_instances
        } else {
            sorted_instances
                .into_iter()
                .filter(|inst| {
                    inst.project
                        .as_deref()
                        .map(|p| p == proj)
                        .unwrap_or(true)
                })
                .collect()
        }
    } else {
        sorted_instances
    };

    let unread_counts = get_unread_counts_batch(db, &sorted_instances);

    if names_output {
        for data in &sorted_instances {
            println!("{}", get_full_name(data));
        }
        return 0;
    }

    if json_output || format_template.is_some() {
        let mut result_list: Vec<serde_json::Value> = Vec::new();

        for data in &sorted_instances {
            let full_name = get_full_name(data);
            let cs = get_instance_status(data, db);
            let (status, description, age_seconds) = (cs.status, cs.description, cs.age_seconds);

            // Get binding status
            let hooks_bound = db.has_session_binding(&data.name);
            let process_bound = db.has_process_binding_for_instance(&data.name);

            // Parse launch_context JSON
            let launch_context: serde_json::Value = data
                .launch_context
                .as_deref()
                .filter(|s| !s.is_empty())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(serde_json::json!({}));

            let payload = serde_json::json!({
                "name": full_name,
                "status": status,
                "status_context": data.status_context,
                "status_detail": data.status_detail,
                "status_age_seconds": age_seconds,
                "description": description,
                "unread_count": unread_counts.get(&data.name).copied().unwrap_or(0),
                "headless": data.background != 0,
                "session_id": data.session_id.as_deref().unwrap_or(""),
                "directory": data.directory,
                "parent_name": data.parent_name,
                "agent_id": data.agent_id,
                "background_log_file": if data.background_log_file.is_empty() { None } else { Some(&data.background_log_file) },
                "transcript_path": if data.transcript_path.is_empty() { None } else { Some(&data.transcript_path) },
                "created_at": data.created_at,
                "tag": data.tag,
                "tool": data.tool,
                "base_name": data.name,
                "hooks_bound": hooks_bound,
                "process_bound": process_bound,
                "launch_context": launch_context,
            });
            result_list.push(payload);
        }

        if let Some(ref template) = format_template {
            // Validate template keys against first payload (error on unknown fields)
            if let Some(first) = result_list.first() {
                if let Some(obj) = first.as_object() {
                    // Find all {key} placeholders in template
                    let mut i = 0;
                    let bytes = template.as_bytes();
                    while i < bytes.len() {
                        if bytes[i] == b'{' {
                            if let Some(end) = template[i + 1..].find('}') {
                                let key = &template[i + 1..i + 1 + end];
                                if !key.is_empty() && !obj.contains_key(key) {
                                    eprintln!(
                                        "Error: unknown field '{{{}}}' in --format template",
                                        key
                                    );
                                    return 1;
                                }
                                i += end + 2;
                                continue;
                            }
                        }
                        i += 1;
                    }
                }
            }
            for payload in &result_list {
                let obj = payload.as_object().unwrap();
                let mut line = template.clone();
                for (key, val) in obj {
                    let replacement = match val {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Null => String::new(),
                        other => other.to_string(),
                    };
                    line = line.replace(&format!("{{{key}}}"), &replacement);
                }
                println!("{line}");
            }
        } else {
            println!(
                "{}",
                serde_json::to_string(&result_list).unwrap_or_default()
            );
        }
        return 0;
    }

    // Human-readable output
    let display_name = if let Some(ref name) = current_name {
        if name != SENDER {
            if let Ok(Some(data)) = db.get_instance_full(name) {
                get_full_name(&data)
            } else {
                name.clone()
            }
        } else {
            name.clone()
        }
    } else {
        String::new()
    };

    if !display_name.is_empty() {
        println!("Your name: {display_name}");
    } else {
        println!("Your name: (not participating)");
    }
    println!();

    // Check if multiple tool types exist
    let mut tool_types = std::collections::HashSet::new();
    for data in &sorted_instances {
        tool_types.insert(data.tool.clone());
    }
    let show_tool = tool_types.len() > 1;

    // Check if multiple directories
    let mut directories = std::collections::HashSet::new();
    for data in &sorted_instances {
        if !data.directory.is_empty() {
            directories.insert(data.directory.clone());
        }
    }

    // Compute name column width
    let mut max_name_len = 0;
    for data in &sorted_instances {
        let mut n = get_full_name(data).len();
        if data.background != 0 {
            n += 11; // " [headless]"
        }
        if is_remote_instance(data) {
            n += 9; // " [remote]"
        }
        let uc = unread_counts.get(&data.name).copied().unwrap_or(0);
        if uc > 0 {
            n += format!(" +{uc}").len();
        }
        max_name_len = max_name_len.max(n);
    }
    let name_col_width = (max_name_len + 2).max(14);

    for data in &sorted_instances {
        let name = get_full_name(data);
        let cs = get_instance_status(data, db);
        let (status, age_str, description) = (cs.status, cs.age_string, cs.description);
        let icon = status_icon(&status);

        let age_display = if age_str == "now" {
            age_str.clone()
        } else if !age_str.is_empty() {
            format!("{age_str} ago")
        } else {
            String::new()
        };

        let desc_sep = if !description.is_empty() { ": " } else { "" };

        // Tool prefix — binding state encoding:
        // UPPER = pty+hooks, lower = hooks only, UPPER* = pty only, lower* = no binding
        let tool_prefix = if show_tool {
            let hooks_bound = db.has_session_binding(&data.name);
            let process_bound = db.has_process_binding_for_instance(&data.name);
            let tool_display = if data.tool == "adhoc" {
                "ad-hoc".to_string()
            } else if process_bound && hooks_bound {
                data.tool.to_uppercase()
            } else if process_bound {
                format!("{}*", data.tool.to_uppercase())
            } else if hooks_bound {
                data.tool.to_lowercase()
            } else {
                format!("{}*", data.tool.to_lowercase())
            };
            let padded = format!("[{tool_display}]");
            format!("{padded:<10}")
        } else {
            String::new()
        };

        // Badges
        let headless_badge = if data.background != 0 {
            " [headless]"
        } else {
            ""
        };
        let remote_badge = if is_remote_instance(data) {
            " [remote]"
        } else {
            ""
        };

        // Unread
        let unread = unread_counts.get(&data.name).copied().unwrap_or(0);
        let unread_str = if unread > 0 {
            format!(" +{unread}")
        } else {
            String::new()
        };

        // Listening-since suffix: show idle duration for listening agents idle >= 60s
        let listening_since = if status == ST_LISTENING && cs.age_seconds >= 60 {
            format!(" since {}", format_age(cs.age_seconds))
        } else {
            String::new()
        };

        // Subagent timeout marker: show countdown when < 10s remaining
        let timeout_marker = if status == ST_LISTENING && data.parent_session_id.is_some() {
            let timeout = if let Some(ref parent_name) = data.parent_name {
                db.get_instance_full(parent_name)
                    .ok()
                    .flatten()
                    .and_then(|p| p.subagent_timeout)
            } else {
                None
            }
            .unwrap_or_else(|| crate::config::load_config_snapshot().core.subagent_timeout);
            let remaining = timeout.saturating_sub(cs.age_seconds);
            if remaining > 0 && remaining < 10 {
                format!(" \u{23f1} {remaining}s")
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        let name_part = format!("{name}{headless_badge}{remote_badge}{unread_str}");
        let status_text =
            format!("{age_display}{desc_sep}{description}{listening_since}{timeout_marker}");

        println!(
            "{tool_prefix}{icon} {name_part:<width$}{status_text}",
            width = name_col_width
        );

        if verbose_output {
            let session_id = data.session_id.as_deref().unwrap_or("(none)");
            let directory_display = if data.directory.is_empty() {
                "(none)".to_string()
            } else {
                shorten_path_max(&data.directory, 60)
            };
            let parent = data.parent_name.as_deref().unwrap_or("(none)");
            let tool_display = if data.tool == "adhoc" {
                "ad-hoc"
            } else {
                &data.tool
            };

            let created_str = if data.created_at > 0.0 {
                let now = crate::shared::time::now_epoch_f64();
                let age_f = now - data.created_at;
                // format_age takes i64 where 0 → "now". Use f64 threshold
                // so sub-second ages display as "0s" instead of "now".
                let age_str = if age_f <= 0.0 {
                    "now".to_string()
                } else {
                    let secs = age_f as i64;
                    if secs < 60 {
                        format!("{secs}s")
                    } else {
                        format_age(secs)
                    }
                };
                format!("{age_str} ago")
            } else {
                "(unknown)".to_string()
            };

            println!("    session_id:   {session_id}");
            println!("    tool:         {tool_display}");
            println!("    created:      {created_str}");
            println!("    directory:    {directory_display}");

            if parent != "(none)" {
                println!("    parent:       {parent}");
                let agent_id = data.agent_id.as_deref().unwrap_or("(none)");
                println!("    agent_id:     {agent_id}");
            }

            // Binding status
            let hooks_bound = db.has_session_binding(&data.name);
            let process_bound = db.has_process_binding_for_instance(&data.name);
            let bind_str = match (hooks_bound, process_bound) {
                (true, true) => "hooks, pty",
                (true, false) => "hooks",
                (false, true) => "pty",
                (false, false) => "none",
            };
            println!("    bindings:     {bind_str}");

            let transcript = if data.transcript_path.is_empty() {
                "(none)".to_string()
            } else {
                shorten_path_max(&data.transcript_path, 60)
            };
            if data.background != 0 && !data.background_log_file.is_empty() {
                println!(
                    "    headless log: {}",
                    shorten_path_max(&data.background_log_file, 60)
                );
            }
            println!("    transcript:   {transcript}");

            if !data.status_detail.is_empty() {
                let detail = if data.status_detail.len() > 60 {
                    let end = (0..=60)
                        .rev()
                        .find(|&i| data.status_detail.is_char_boundary(i))
                        .unwrap_or(0);
                    format!("{}...", &data.status_detail[..end])
                } else {
                    data.status_detail.clone()
                };
                println!("    detail:       {detail}");
            }
            println!();
        }
    }

    if sorted_instances.is_empty() {
        println!("No active agents. Launch one with: hcom claude");
    }

    // Recently stopped summary
    let active_names: std::collections::HashSet<String> =
        sorted_instances.iter().map(|d| d.name.clone()).collect();
    let recently_stopped = get_recently_stopped(db, &active_names);
    if !recently_stopped.is_empty() {
        let names = if recently_stopped.len() <= 5 {
            recently_stopped.join(", ")
        } else {
            format!(
                "{} +{}",
                recently_stopped[..5].join(", "),
                recently_stopped.len() - 5
            )
        };
        println!("\nRecently stopped (10m): {names}");
        println!("  -> hcom list --stopped [name]");
    }

    // Hint about archives if no instances
    if sorted_instances.is_empty() {
        let archive_dir = crate::paths::hcom_dir().join("archive");
        if archive_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&archive_dir) {
                let archive_count = entries
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.file_name()
                            .to_str()
                            .map(|s| s.starts_with("session-"))
                            .unwrap_or(false)
                    })
                    .count();
                if archive_count > 0 {
                    let plural = if archive_count != 1 { "s" } else { "" };
                    println!("({archive_count} archived session{plural} - run: hcom archive)");
                }
            }
        }
    }

    0
}

/// Extract a field value from a JSON payload, normalizing booleans to "1"/"0".
fn extract_field_value(payload: &serde_json::Value, field: &str) -> String {
    match payload.get(field) {
        Some(serde_json::Value::Bool(b)) => if *b { "1" } else { "0" }.to_string(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Null) => String::new(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// Print shell-export format for `hcom list --sh`.
fn print_sh_exports(payload: &serde_json::Value) {
    let name = payload.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let session_id = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let status = payload
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let directory = payload
        .get("directory")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    println!("export HCOM_INSTANCE_NAME={}", shell_quote(name));
    println!("export HCOM_SID={}", shell_quote(session_id));
    println!("export HCOM_STATUS={}", shell_quote(status));
    println!("export HCOM_DIRECTORY={}", shell_quote(directory));
}

use crate::tools::args_common::shell_quote;

/// `hcom list --stopped [name] [--all] [--last N]` — show stopped instances from life events.
/// Without a name: shows recent stopped (default last 20, use --all for unlimited).
/// With a name: shows details for that specific stopped instance.
/// Uses human-friendly formatting rather than raw JSON for readability.
fn cmd_list_stopped(db: &HcomDb, args: &ListArgs) -> i32 {
    use rusqlite::params;

    let show_all = args.all;
    let last_n: usize = args.last.unwrap_or(20);
    let filter_name = args.name.as_deref();

    let now = crate::shared::time::now_epoch_f64();

    let limit = if show_all { 10000 } else { last_n };

    let (query, param) = if let Some(name) = filter_name {
        let name = crate::identity::resolve_display_name_or_stopped(db, name)
            .unwrap_or_else(|| name.to_string());
        // Fix: fetch up to 10000 events for named instance (was LIMIT 1)
        (
            "SELECT instance, timestamp, data FROM events
             WHERE type = 'life' AND json_extract(data, '$.action') = 'stopped'
             AND instance = ?
             ORDER BY id DESC LIMIT 10000"
                .to_string(),
            name,
        )
    } else {
        (
            format!(
                "SELECT instance, timestamp, data FROM events
                 WHERE type = 'life' AND json_extract(data, '$.action') = 'stopped'
                 ORDER BY id DESC LIMIT {limit}"
            ),
            String::new(),
        )
    };

    let Ok(mut stmt) = db.conn().prepare(&query) else {
        eprintln!("Error: failed to query stopped events");
        return 1;
    };

    struct StoppedEntry {
        instance: String,
        timestamp: String,
        data: String,
    }

    let row_mapper = |row: &rusqlite::Row| -> rusqlite::Result<StoppedEntry> {
        Ok(StoppedEntry {
            instance: row.get(0)?,
            timestamp: row.get(1)?,
            data: row.get(2)?,
        })
    };

    let entries: Vec<StoppedEntry> = if filter_name.is_some() {
        stmt.query_map(params![param], row_mapper)
    } else {
        stmt.query_map([], row_mapper)
    }
    .ok()
    .into_iter()
    .flatten()
    .filter_map(|r| r.ok())
    .collect();

    if entries.is_empty() {
        if let Some(name) = filter_name {
            println!("No stopped events found for '{name}'");
        } else {
            println!("No recently stopped agents (last 60m)");
        }
        return 0;
    }

    if filter_name.is_some() {
        // Detailed view for a single instance (show all stop events, not just 1)
        let entry = &entries[0];
        let data: serde_json::Value = serde_json::from_str(&entry.data).unwrap_or_default();
        let snapshot = &data["snapshot"];
        println!("Stopped: {}", entry.instance);
        println!("  Time:       {}", &entry.timestamp);
        if let Some(by) = data["by"].as_str() {
            println!("  By:         {by}");
        }
        if let Some(reason) = data["reason"].as_str() {
            println!("  Reason:     {reason}");
        }
        if let Some(tool) = snapshot["tool"].as_str() {
            println!("  Tool:       {tool}");
        }
        if let Some(tag) = snapshot["tag"].as_str() {
            if !tag.is_empty() {
                println!("  Tag:        {tag}");
            }
        }
        if let Some(dir) = snapshot["directory"].as_str() {
            println!("  Directory:  {dir}");
        }
        if let Some(sid) = snapshot["session_id"].as_str() {
            if !sid.is_empty() {
                println!("  Session:    {sid}");
            }
        }
        if let Some(tp) = snapshot["transcript_path"].as_str() {
            if !tp.is_empty() {
                println!("  Transcript: {tp}");
            }
        }
        println!("\n  Resume: hcom r {}", entry.instance);

        // Show history if multiple stop events
        if entries.len() > 1 {
            println!("\n  Stop history ({} events):", entries.len());
            for (i, e) in entries.iter().enumerate() {
                let d: serde_json::Value = serde_json::from_str(&e.data).unwrap_or_default();
                let reason = d["reason"].as_str().unwrap_or("");
                let by = d["by"].as_str().unwrap_or("");
                let age = if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&e.timestamp) {
                    let event_epoch = dt.timestamp() as f64;
                    format_age((now - event_epoch) as i64)
                } else {
                    e.timestamp.clone()
                };
                let by_part = if by.is_empty() {
                    String::new()
                } else {
                    format!(" by:{by}")
                };
                let marker = if i == 0 { " (latest)" } else { "" };
                println!("    {age} ago  [{reason}{by_part}]{marker}");
            }
        }
    } else {
        // Summary table
        let header = if show_all {
            format!("Stopped agents (all, showing {}):", entries.len())
        } else {
            format!("Stopped agents (last {last_n}):")
        };
        println!("{header}\n");
        for entry in &entries {
            let data: serde_json::Value = serde_json::from_str(&entry.data).unwrap_or_default();
            let snapshot = &data["snapshot"];
            let tool = snapshot["tool"].as_str().unwrap_or("?");
            let tag = snapshot["tag"].as_str().unwrap_or("");
            let reason = data["reason"].as_str().unwrap_or("");
            let by = data["by"].as_str().unwrap_or("");
            // Parse timestamp for age
            let age = if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&entry.timestamp) {
                let event_epoch = dt.timestamp() as f64;
                format_age((now - event_epoch) as i64)
            } else {
                entry.timestamp.clone()
            };
            let dir = snapshot["directory"].as_str().unwrap_or("");
            let tag_part = if tag.is_empty() {
                String::new()
            } else {
                format!(" tag:{tag}")
            };
            let by_part = if by.is_empty() {
                String::new()
            } else {
                format!(" by:{by}")
            };
            let dir_part = if dir.is_empty() {
                String::new()
            } else {
                format!("  {}", shorten_path_max(dir, 40))
            };
            println!(
                "  {} ({tool}{tag_part}) {age} ago  [{reason}{by_part}]{dir_part}",
                entry.instance
            );
        }
        if !show_all {
            println!("\n  --all: show all  |  --last N: show last N");
        }
        println!("  Details: hcom list --stopped <name>");
        println!("  Resume:  hcom r <name>");
    }

    0
}

/// Get names of recently stopped instances (within 10 minutes).
fn get_recently_stopped(
    db: &HcomDb,
    exclude_active: &std::collections::HashSet<String>,
) -> Vec<String> {
    let now = crate::shared::time::now_epoch_f64();
    let cutoff = now - RECENTLY_STOPPED_WINDOW;
    let cutoff_ts = chrono::DateTime::from_timestamp(cutoff as i64, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
        .unwrap_or_default();

    let Ok(mut stmt) = db.conn().prepare(
        "SELECT DISTINCT instance FROM events
         WHERE type = 'life' AND json_extract(data, '$.action') = 'stopped'
         AND timestamp > ?
         ORDER BY id DESC",
    ) else {
        return vec![];
    };

    stmt.query_map(rusqlite::params![cutoff_ts], |row| row.get::<_, String>(0))
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|r| r.ok())
        .filter(|name| !exclude_active.contains(name))
        .collect()
}
