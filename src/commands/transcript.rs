//! `hcom transcript` command — view and search agent conversation transcripts.
//!
//!
//! Supports:
//! - View transcript: `hcom transcript @instance [N | N-M] [--full] [--detailed] [--json] [--last N]`
//! - Timeline: `hcom transcript timeline [--last N] [--full] [--json]`
//! - Search: `hcom transcript search "pattern" [--live] [--all] [--limit N] [--agent TYPE]`

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::db::HcomDb;
use crate::shared::CommandContext;
use crate::transcript::{self, Exchange, ReadOptions, format_exchanges, summarize_action};

/// Parsed arguments for `hcom transcript`.
#[derive(clap::Parser, Debug)]
#[command(name = "transcript", about = "View and search transcripts")]
pub struct TranscriptArgs {
    /// Subcommand (search, timeline) or view mode
    #[command(subcommand)]
    pub subcmd: Option<TranscriptSubcmd>,

    /// Target instance name (with or without @)
    pub name: Option<String>,
    /// Exchange range (e.g., "5" or "5-10")
    pub range_positional: Option<String>,

    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Full output (no streamlining)
    #[arg(long)]
    pub full: bool,
    /// Detailed output (include tool details)
    #[arg(long)]
    pub detailed: bool,
    /// Last N exchanges
    #[arg(long)]
    pub last: Option<usize>,
    /// Exchange range (flag form)
    #[arg(long = "range")]
    pub range_flag: Option<String>,
}

#[derive(clap::Subcommand, Debug)]
pub enum TranscriptSubcmd {
    /// Search transcripts for a pattern
    Search(TranscriptSearchArgs),
    /// Show timeline of all agents' recent activity
    Timeline(TranscriptTimelineArgs),
}

/// Args for `hcom transcript search`.
#[derive(clap::Args, Debug)]
pub struct TranscriptSearchArgs {
    /// Search pattern (regex)
    pub pattern: String,
    /// Live-watch mode
    #[arg(long)]
    pub live: bool,
    /// Search all transcripts on disk (not just tracked instances)
    #[arg(long)]
    pub all: bool,
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Exclude own transcript from search results
    #[arg(long)]
    pub exclude_self: bool,
    /// Max results (default: 20)
    #[arg(long, default_value = "20")]
    pub limit: usize,
    /// Filter by agent type (claude, gemini, codex, opencode)
    #[arg(long)]
    pub agent: Option<String>,
}

/// Args for `hcom transcript timeline`.
#[derive(clap::Args, Debug)]
pub struct TranscriptTimelineArgs {
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Full output
    #[arg(long)]
    pub full: bool,
    /// Detailed output
    #[arg(long)]
    pub detailed: bool,
    /// Last N exchanges per agent
    #[arg(long)]
    pub last: Option<usize>,
}

/// Truncate a string to at most `max` bytes at a valid UTF-8 char boundary.
fn truncate_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ── Transcript Path Discovery ────────────────────────────────────────────

/// Get Claude config directory.
pub(crate) fn claude_config_dir() -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".claude"))
}

/// Detect agent type from transcript path.
pub(crate) fn detect_agent_type(path: &str) -> &str {
    if path.contains(".claude") || path.contains("/projects/") {
        "claude"
    } else if path.contains(".gemini") {
        "gemini"
    } else if path.contains(".codex") || path.contains("codex") {
        "codex"
    } else if path.contains("opencode") {
        "opencode"
    } else {
        "unknown"
    }
}

fn transcript_search_key(path: &str, session_id: Option<&str>) -> String {
    format!("{path}\u{0}{}", session_id.unwrap_or(""))
}

fn transcript_agent_matches(filter: Option<&str>, agent: &str) -> bool {
    filter.is_none_or(|f| agent.contains(f) || f.contains(agent))
}

/// Get transcript path for an instance from DB.
fn get_transcript_path(db: &HcomDb, name: &str) -> Option<String> {
    db.conn()
        .query_row(
            "SELECT transcript_path FROM instances WHERE name = ?",
            rusqlite::params![name],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
        .filter(|p| !p.is_empty())
}

/// Build an appropriate error message when transcript resolution fails.
/// Uses resolve_display_name_or_stopped (which already handles prefix matching)
/// to check if the instance exists without a transcript.
fn no_transcript_error(db: &HcomDb, name: &str) -> String {
    if let Some(resolved) = crate::identity::resolve_display_name_or_stopped(db, name) {
        format!(
            "Agent '{}' has no transcript yet — no messages have been exchanged",
            resolved
        )
    } else {
        format!("Agent '{name}' not found")
    }
}

/// Get exchanges from a transcript file using the shared transcript module.
fn get_exchanges(
    path: &str,
    agent: &str,
    last: usize,
    detailed: bool,
    session_id: Option<&str>,
    retry_codex: bool,
) -> Result<Vec<Exchange>, String> {
    let kind = transcript::kind_from_agent_or_path(agent, path);
    let opts = ReadOptions {
        last,
        detailed,
        session_id: session_id.map(|s| s.to_string()),
        allow_codex_retry: retry_codex,
    };
    transcript::read(Path::new(path), kind, &opts)
}

// ── Search ───────────────────────────────────────────────────────────────

/// Correlate transcript file paths to hcom agent names via DB queries.
/// Checks instances table first, then stopped life events.
fn correlate_paths_to_hcom(
    db: &HcomDb,
    targets: &[(String, Option<String>)],
) -> std::collections::HashMap<String, String> {
    let mut result = std::collections::HashMap::new();
    let conn = db.conn();
    let target_keys: std::collections::HashSet<String> = targets
        .iter()
        .map(|(path, session_id)| transcript_search_key(path, session_id.as_deref()))
        .collect();

    // 1. Check current instances
    if let Ok(mut stmt) = conn.prepare(
        "SELECT name, transcript_path, session_id
         FROM instances
         WHERE transcript_path IS NOT NULL",
    ) {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        }) {
            for (name, tp, session_id) in rows.flatten() {
                let key = transcript_search_key(&tp, session_id.as_deref());
                if target_keys.contains(&key) {
                    result.insert(key, name);
                }
            }
        }
    }

    // 2. Check stopped events for paths not yet matched
    if let Ok(mut stmt) = conn.prepare(
        "SELECT instance,
                json_extract(data, '$.snapshot.transcript_path') as tp,
                json_extract(data, '$.snapshot.session_id') as session_id \
         FROM events WHERE type = 'life' \
         AND json_extract(data, '$.action') = 'stopped' \
         AND json_extract(data, '$.snapshot.transcript_path') IS NOT NULL \
         ORDER BY id DESC",
    ) {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        }) {
            for (name, tp, session_id) in rows.flatten() {
                let key = transcript_search_key(&tp, session_id.as_deref());
                if target_keys.contains(&key) && !result.contains_key(&key) {
                    result.insert(key, name);
                }
            }
        }
    }

    result
}

/// Search across transcripts: `hcom transcript search "pattern" [--live] [--all] [--limit N] [--exclude-self]`
fn cmd_transcript_search(
    db: &HcomDb,
    args: &TranscriptSearchArgs,
    ctx: Option<&CommandContext>,
) -> i32 {
    let live_mode = args.live;
    let all_mode = args.all;
    let json_mode = args.json;
    let limit = args.limit;
    let agent_filter = args.agent.as_ref();

    // Resolve self name for --exclude-self
    let ctx_name = if args.exclude_self {
        ctx.and_then(|c| c.identity.as_ref())
            .filter(|id| matches!(id.kind, crate::shared::SenderKind::Instance))
            .map(|id| id.name.clone())
    } else {
        None
    };

    let pattern = &args.pattern;

    // Collect transcript paths: (name, path, agent)
    let mut paths: Vec<(String, String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    if all_mode {
        // --all: search disk-wide directories (not just hcom-tracked instances)
        let mut search_dirs: Vec<PathBuf> = Vec::new();
        let agent_filter = agent_filter.map(|s| s.as_str());
        let opencode_db = if transcript_agent_matches(agent_filter, "opencode") {
            transcript::get_opencode_db_path()
        } else {
            None
        };

        if transcript_agent_matches(agent_filter, "claude") {
            let p = claude_config_dir().join("projects");
            if p.exists() {
                search_dirs.push(p);
            }
        }
        if transcript_agent_matches(agent_filter, "gemini") {
            let home = dirs::home_dir().unwrap_or_default();
            let p = home.join(".gemini");
            if p.exists() {
                search_dirs.push(p);
            }
        }
        if transcript_agent_matches(agent_filter, "codex") {
            let home = dirs::home_dir().unwrap_or_default();
            let p = home.join(".codex").join("sessions");
            if p.exists() {
                search_dirs.push(p);
            }
        }

        if search_dirs.is_empty() && opencode_db.is_none() {
            println!("No transcript directories found on disk.");
            return 0;
        }

        // Phase 1: find matching files with rg -l (recursive, *.jsonl/*.json)
        let mut cmd = std::process::Command::new("rg");
        cmd.args(["-l", "--glob", "*.jsonl", "--glob", "*.json", pattern]);
        for d in &search_dirs {
            cmd.arg(d);
        }
        let output = cmd.output();
        let matching_files: Vec<String> = match output {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| l.to_string())
                .collect(),
            _ => Vec::new(),
        };

        let opencode_matches = opencode_db
            .as_deref()
            .map(|db_path| transcript::search_opencode_sessions(db_path, pattern, limit))
            .transpose();
        let opencode_matches = match opencode_matches {
            Ok(Some(matches)) => matches,
            Ok(None) => Vec::new(),
            Err(err) => {
                eprintln!("Error: {err}");
                return 1;
            }
        };

        if matching_files.is_empty() && opencode_matches.is_empty() {
            if json_mode {
                println!("{}", json!({"count": 0, "results": [], "scope": "all"}));
            } else {
                println!("No matches for \"{pattern}\"");
            }
            return 0;
        }

        // Correlate transcript paths/session IDs to hcom names via DB.
        let mut targets: Vec<(String, Option<String>)> = matching_files
            .iter()
            .cloned()
            .map(|path| (path, None))
            .collect();
        targets.extend(
            opencode_matches
                .iter()
                .filter_map(|m| m.session_id.clone().map(|sid| (m.path.clone(), Some(sid)))),
        );
        let path_to_hcom = correlate_paths_to_hcom(db, &targets);

        // Extract line-level matches from each file
        let mut results = Vec::new();
        for file_path in &matching_files {
            if results.len() >= limit {
                break;
            }
            let agent = detect_agent_type(file_path);
            if let Some(af) = agent_filter {
                if !agent.contains(af) {
                    continue;
                }
            }
            let hcom_name = path_to_hcom
                .get(&transcript_search_key(file_path, None))
                .cloned()
                .unwrap_or_default();

            let remaining = limit - results.len();
            let out = std::process::Command::new("rg")
                .args([
                    "-n",
                    "--max-count",
                    &remaining.to_string(),
                    "--max-columns",
                    "500",
                    pattern,
                    file_path,
                ])
                .output();
            if let Ok(out) = out {
                if out.status.success() {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let lines: Vec<&str> = stdout.lines().collect();
                    let match_count = lines.len();
                    if match_count > 0 {
                        let first_line = lines[0];
                        let (line_num, snippet) = if let Some(colon_pos) = first_line.find(':') {
                            let num = first_line[..colon_pos].parse::<usize>().unwrap_or(0);
                            let text = &first_line[colon_pos + 1..];
                            let text = truncate_str(text, 100);
                            (num, text.to_string())
                        } else {
                            (0, first_line.to_string())
                        };

                        results.push(json!({
                            "hcom_name": if hcom_name.is_empty() { serde_json::Value::Null } else { json!(hcom_name) },
                            "agent": agent,
                            "path": file_path,
                            "line": line_num,
                            "text": snippet,
                            "matches": match_count,
                        }));
                    }
                }
            }
        }

        for opencode_match in &opencode_matches {
            if results.len() >= limit {
                break;
            }
            let hcom_name = path_to_hcom
                .get(&transcript_search_key(
                    &opencode_match.path,
                    opencode_match.session_id.as_deref(),
                ))
                .cloned()
                .unwrap_or_default();
            results.push(json!({
                "hcom_name": if hcom_name.is_empty() { serde_json::Value::Null } else { json!(hcom_name) },
                "agent": opencode_match.agent,
                "path": opencode_match.path,
                "line": opencode_match.line,
                "text": opencode_match.text,
                "matches": opencode_match.matches,
                "session_id": opencode_match.session_id,
                "label": opencode_match.label,
            }));
        }

        let scope_label = "";
        if json_mode {
            println!(
                "{}",
                json!({"count": results.len(), "results": results, "scope": "all"})
            );
        } else {
            if results.is_empty() {
                println!("No matches for \"{pattern}\"");
            } else {
                let _ = scope_label;
                println!(
                    "Found matches in {} transcripts (all on disk):",
                    results.len()
                );
                for r in &results {
                    let path = r["path"].as_str().unwrap_or("");
                    let agent = r["agent"].as_str().unwrap_or("?");
                    let line = r["line"].as_u64().unwrap_or(0);
                    let matches = r["matches"].as_u64().unwrap_or(0);
                    let snippet = r["text"].as_str().unwrap_or("");
                    let label = r["label"].as_str().unwrap_or("");
                    let session_id = r["session_id"].as_str().unwrap_or("");
                    let short_path = path
                        .split('/')
                        .rev()
                        .take(3)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join("/");
                    let name_part = r["hcom_name"]
                        .as_str()
                        .map(|n| format!(" ({n})"))
                        .unwrap_or_default();
                    println!("  [{agent}]{name_part} .../{short_path}:{line}  ({matches} matches)");
                    if !label.is_empty() || !session_id.is_empty() {
                        let mut details = Vec::new();
                        if !label.is_empty() {
                            details.push(label.to_string());
                        }
                        if !session_id.is_empty() {
                            details.push(session_id.to_string());
                        }
                        println!("    {}", details.join(" | "));
                    }
                    if !snippet.is_empty() {
                        println!("    {snippet}");
                    }
                }
            }
        }
        return 0;
    } else {
        // Active instances
        if let Ok(mut stmt) = db.conn().prepare(
            "SELECT name, transcript_path, tool FROM instances WHERE transcript_path IS NOT NULL AND transcript_path != ''"
        ) {
            if let Ok(rows) = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            }) {
                for (name, path, tool) in rows.flatten() {
                    if let Some(agent) = agent_filter {
                        if !tool.contains(agent.as_str()) { continue; }
                    }
                    if args.exclude_self && ctx_name.as_deref() == Some(name.as_str()) { continue; }
                    seen.insert(name.clone());
                    paths.push((name, path, tool));
                }
            }
        }

        // Stopped instances from life event snapshots (C2/C3 fix)
        if !live_mode {
            if let Ok(mut stmt) = db.conn().prepare(
                "SELECT instance, json_extract(data, '$.snapshot.transcript_path'), json_extract(data, '$.snapshot.tool') FROM events WHERE type = 'life' AND json_extract(data, '$.action') = 'stopped' AND json_extract(data, '$.snapshot.transcript_path') IS NOT NULL"
            ) {
                if let Ok(rows) = stmt.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                }) {
                    for (name, path, tool) in rows.flatten() {
                        if seen.contains(&name) { continue; }
                        if let Some(agent) = agent_filter {
                            if !tool.contains(agent.as_str()) { continue; }
                        }
                        if args.exclude_self && ctx_name.as_deref() == Some(name.as_str()) { continue; }
                        seen.insert(name.clone());
                        paths.push((name, path, tool));
                    }
                }
            }
        }
    }

    // Search using ripgrep (with line-level matches + snippets) — hcom-tracked/live paths
    let mut results = Vec::new();
    for (name, path, agent) in &paths {
        if !Path::new(path).exists() {
            continue;
        }

        // Use rg for line-level matches with context
        let remaining = limit - results.len();
        let output = std::process::Command::new("rg")
            .args([
                "-n",
                "--max-count",
                &remaining.to_string(),
                "--max-columns",
                "500",
                pattern,
                path,
            ])
            .output()
            .or_else(|_| {
                std::process::Command::new("grep")
                    .args(["-n", "-m", &remaining.to_string(), pattern, path])
                    .output()
            });

        if let Ok(out) = output {
            if out.status.success() {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let lines: Vec<&str> = stdout.lines().collect();
                let match_count = lines.len();
                if match_count > 0 {
                    // Extract first match line number and snippet
                    let first_line = lines[0];
                    let (line_num, snippet) = if let Some(colon_pos) = first_line.find(':') {
                        let num = first_line[..colon_pos].parse::<usize>().unwrap_or(0);
                        let text = &first_line[colon_pos + 1..];
                        let text = truncate_str(text, 100);
                        (num, text.to_string())
                    } else {
                        (0, first_line.to_string())
                    };

                    results.push(json!({
                        "hcom_name": name,
                        "agent": agent,
                        "path": path,
                        "line": line_num,
                        "text": snippet,
                        "matches": match_count,
                    }));
                }
            }
        }

        if results.len() >= limit {
            break;
        }
    }

    let scope_label = if live_mode {
        " (live agents)"
    } else if all_mode {
        ""
    } else {
        " (hcom-tracked)"
    };

    if json_mode {
        println!(
            "{}",
            json!({"count": results.len(), "results": results, "scope": if live_mode {"live"} else if all_mode {"all"} else {"hcom"}})
        );
    } else {
        if results.is_empty() {
            println!("No matches for \"{pattern}\"");
            return 0;
        }
        let limit_hit = results.len() >= limit;
        if limit_hit {
            println!(
                "Showing {} matches (limit {}){scope_label}:\n",
                results.len(),
                limit
            );
        } else {
            println!("Found {} matches{scope_label}:\n", results.len());
        }
        for result in &results {
            let hcom_name = result
                .get("hcom_name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let agent = result.get("agent").and_then(|v| v.as_str()).unwrap_or("");
            let path = result.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let line = result.get("line").and_then(|v| v.as_u64()).unwrap_or(0);
            let snippet = result.get("text").and_then(|v| v.as_str()).unwrap_or("");

            let path_display = if path.len() > 60 {
                let mut start = path.len() - 57;
                while start < path.len() && !path.is_char_boundary(start) {
                    start += 1;
                }
                format!("...{}", &path[start..])
            } else {
                path.to_string()
            };

            println!("[{agent}:{hcom_name}] {path_display}:{line}");
            let snippet_clean = snippet.replace('\n', " ");
            let snippet_short = if snippet_clean.len() > 100 {
                format!("{}...", truncate_str(&snippet_clean, 100))
            } else {
                snippet_clean
            };
            println!("    {snippet_short}\n");
        }
    }

    0
}

/// Timeline: `hcom transcript timeline [--last N] [--full] [--json]`
fn cmd_transcript_timeline(db: &HcomDb, args: &TranscriptTimelineArgs) -> i32 {
    let json_mode = args.json;
    let full_mode = args.full;
    let detailed = args.detailed;
    let last_n = args.last.unwrap_or(10);

    // Collect all transcript paths (active + stopped sessions, C3 fix)
    let mut all_entries: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Active instances
    if let Ok(mut stmt) = db.conn().prepare(
        "SELECT name, transcript_path, tool, session_id FROM instances WHERE transcript_path IS NOT NULL AND transcript_path != ''"
    ) {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        }) {
            for (name, path, tool, sid) in rows.flatten() {
                seen.insert(name.clone());
                if let Ok(exchanges) =
                    get_exchanges(&path, &tool, last_n, detailed, sid.as_deref(), true)
                {
                    for ex in exchanges {
                        all_entries.push(json!({
                            "instance": name,
                            "position": ex.position,
                            "user": ex.user,
                            "action": if full_mode { ex.action.clone() } else { summarize_action(&ex.action) },
                            "timestamp": ex.timestamp,
                            "files": ex.files,
                        }));
                    }
                }
            }
        }
    }

    // Stopped instances from life event snapshots
    if let Ok(mut stmt) = db.conn().prepare(
        "SELECT instance, json_extract(data, '$.snapshot.transcript_path'), json_extract(data, '$.snapshot.tool'), json_extract(data, '$.snapshot.session_id') FROM events WHERE type = 'life' AND json_extract(data, '$.action') = 'stopped' AND json_extract(data, '$.snapshot.transcript_path') IS NOT NULL"
    ) {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        }) {
            for (name, path, tool, sid) in rows.flatten() {
                if seen.contains(&name) { continue; }
                seen.insert(name.clone());
                if let Ok(exchanges) =
                    get_exchanges(&path, &tool, last_n, detailed, sid.as_deref(), true)
                {
                    for ex in exchanges {
                        all_entries.push(json!({
                            "instance": name,
                            "position": ex.position,
                            "user": ex.user,
                            "action": if full_mode { ex.action.clone() } else { summarize_action(&ex.action) },
                            "timestamp": ex.timestamp,
                            "files": ex.files,
                        }));
                    }
                }
            }
        }
    }

    // Sort by timestamp (most recent first)
    all_entries.sort_by(|a, b| {
        let ts_a = a.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
        let ts_b = b.get("timestamp").and_then(|v| v.as_str()).unwrap_or("");
        ts_b.cmp(ts_a) // Reverse order
    });

    // Limit
    if all_entries.len() > last_n {
        all_entries.truncate(last_n);
    }

    if json_mode {
        println!(
            "{}",
            serde_json::to_string_pretty(&all_entries).unwrap_or_default()
        );
        return 0;
    }

    if all_entries.is_empty() {
        println!("No transcript entries found");
        return 0;
    }

    //
    println!("Timeline ({} exchanges):\n", all_entries.len());
    for entry in &all_entries {
        let inst = entry.get("instance").and_then(|v| v.as_str()).unwrap_or("");
        let ts = entry
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let user = entry.get("user").and_then(|v| v.as_str()).unwrap_or("");
        let action = entry.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let files = entry.get("files").and_then(|v| v.as_array());

        let ts_short = if ts.contains('T') {
            ts.get(11..16).unwrap_or("??:??")
        } else if ts.len() >= 5 {
            ts.get(..5).unwrap_or("??:??")
        } else {
            "??:??"
        };

        let user_display = if user.len() > 80 {
            format!("{}...", truncate_str(user, 77))
        } else {
            user.to_string()
        };

        println!("[{ts_short}] \"{user_display}\"");

        if full_mode {
            for action_line in action.lines().take(10) {
                println!("  {action_line}");
            }
            let line_count = action.lines().count();
            if line_count > 10 {
                println!("  ... (+{} lines)", line_count - 10);
            }
        } else {
            let action_short = summarize_action(action);
            let action_display = if action_short.len() > 100 {
                format!("{}...", truncate_str(&action_short, 97))
            } else {
                action_short
            };
            println!("  → {action_display}");
        }

        if let Some(file_arr) = files {
            let file_strs: Vec<&str> = file_arr.iter().take(5).filter_map(|v| v.as_str()).collect();
            if !file_strs.is_empty() {
                println!("  Files: {}", file_strs.join(", "));
            }
        }

        // Command line (instance reference for navigation)
        println!(
            "  hcom transcript @{inst} {}",
            entry.get("position").and_then(|v| v.as_u64()).unwrap_or(1)
        );
        println!();
    }

    0
}

// ── Main Entry Point ─────────────────────────────────────────────────────

/// Main entry point for `hcom transcript` command.
pub fn cmd_transcript(db: &HcomDb, args: &TranscriptArgs, ctx: Option<&CommandContext>) -> i32 {
    // Handle subcommands
    match &args.subcmd {
        Some(TranscriptSubcmd::Search(search_args)) => {
            return cmd_transcript_search(db, search_args, ctx);
        }
        Some(TranscriptSubcmd::Timeline(timeline_args)) => {
            return cmd_transcript_timeline(db, timeline_args);
        }
        None => {}
    }

    let json_mode = args.json;
    let full_mode = args.full;
    let detailed = args.detailed;
    let last_n = args.last.unwrap_or(10);

    if let Some(ref name) = args.name {
        let stripped = name.strip_prefix('@').unwrap_or(name);
        let resolved = crate::identity::resolve_display_name_or_stopped(db, stripped)
            .unwrap_or_else(|| stripped.to_string());
        if let Some((base_name, device)) = crate::relay::control::split_device_suffix(&resolved) {
            return crate::relay::control::dispatch_remote_and_print(
                db,
                device,
                Some(&resolved),
                crate::relay::control::rpc_action::TRANSCRIPT,
                &json!({
                    "target": base_name,
                    "last": last_n,
                    "range": args.range_flag.as_ref().or(args.range_positional.as_ref()),
                    "json": json_mode,
                    "full": full_mode,
                    "detailed": detailed,
                }),
                crate::relay::control::RPC_DEFAULT_TIMEOUT,
                "content",
                "No remote transcript content",
            );
        }
    }

    // Resolve target and range from positional args
    let mut target = None;
    let mut range_str: Option<String> = args.range_flag.clone();

    if let Some(ref name) = args.name {
        let stripped = name.strip_prefix('@').unwrap_or(name);
        // Check if it looks like a range (digits and hyphens)
        if stripped.chars().all(|c| c.is_ascii_digit() || c == '-')
            && stripped.chars().any(|c| c.is_ascii_digit())
        {
            if range_str.is_none() {
                range_str = Some(stripped.to_string());
            }
        } else {
            target = Some(stripped.to_string());
        }
    }

    if let Some(ref range_pos) = args.range_positional {
        if range_str.is_none() {
            range_str = Some(range_pos.clone());
        }
    }

    // Resolve target to transcript path
    let (instance_name, transcript_path, agent_type, session_id) = if let Some(ref name) = target {
        // Try direct match
        let resolved = resolve_instance_transcript(db, name);
        match resolved {
            Some(r) => r,
            None => {
                eprintln!("Error: {}", no_transcript_error(db, name));
                return 1;
            }
        }
    } else if let Some(id) = ctx.and_then(|c| c.identity.as_ref()) {
        // Default to self
        match resolve_instance_transcript(db, &id.name) {
            Some(r) => r,
            None => {
                eprintln!("Error: No transcript available for current instance");
                return 1;
            }
        }
    } else {
        eprintln!("Usage: hcom transcript @instance [N | N-M] [--full] [--json]");
        return 1;
    };

    // Parse range
    let (range_start, range_end) = if let Some(ref r) = range_str {
        parse_range(r)
    } else {
        (None, None)
    };

    // Get exchanges
    let effective_last = if range_start.is_some() {
        usize::MAX
    } else {
        last_n
    };
    let exchanges = match get_exchanges(
        &transcript_path,
        &agent_type,
        effective_last,
        detailed,
        session_id.as_deref(),
        true,
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Error: {e}");
            return 1;
        }
    };

    // Apply range filter
    let filtered: Vec<&Exchange> = if let Some(start) = range_start {
        let end = range_end.unwrap_or(start);
        exchanges
            .iter()
            .filter(|e| e.position >= start && e.position <= end)
            .collect()
    } else {
        exchanges.iter().collect()
    };

    if json_mode {
        let json_output: Vec<Value> = filtered
            .iter()
            .map(|ex| {
                let mut obj = json!({
                    "position": ex.position,
                    "user": ex.user,
                    "action": ex.action,
                    "files": ex.files,
                    "timestamp": ex.timestamp,
                });
                if detailed {
                    obj["tools"] = json!(
                        ex.tools
                            .iter()
                            .map(|t| {
                                let mut tool = json!({
                                    "name": t.name,
                                    "is_error": t.is_error,
                                });
                                if let Some(ref f) = t.file {
                                    tool["file"] = json!(f);
                                }
                                if let Some(ref c) = t.command {
                                    tool["command"] = json!(c);
                                }
                                tool
                            })
                            .collect::<Vec<_>>()
                    );
                    obj["edits"] = json!(ex.edits);
                    obj["errors"] = json!(ex.errors);
                    obj["ended_on_error"] = json!(ex.ended_on_error);
                }
                obj
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json_output).unwrap_or_default()
        );
        return 0;
    }

    if filtered.is_empty() {
        println!("No exchanges found");
        return 0;
    }

    // Header: "Recent conversation (N exchanges, X-Y of Z) - @instance:"
    let first_pos = filtered.first().map(|e| e.position).unwrap_or(1);
    let last_pos = filtered.last().map(|e| e.position).unwrap_or(1);
    println!(
        "Recent conversation ({} exchanges, {}-{} of {}) - @{}:\n",
        filtered.len(),
        first_pos,
        last_pos,
        exchanges.len(),
        instance_name,
    );

    let owned: Vec<Exchange> = filtered.into_iter().cloned().collect();
    let formatted = format_exchanges(&owned, &instance_name, full_mode, detailed);
    println!("{formatted}");

    0
}

/// Display options for transcript rendering.
pub struct TranscriptRenderOpts<'a> {
    pub range: Option<&'a str>,
    pub last_n: usize,
    pub json_mode: bool,
    pub full_mode: bool,
    pub detailed: bool,
    pub retry_codex: bool,
}

impl Default for TranscriptRenderOpts<'_> {
    fn default() -> Self {
        Self {
            range: None,
            last_n: 10,
            json_mode: false,
            full_mode: false,
            detailed: false,
            retry_codex: true,
        }
    }
}

pub fn render_instance_transcript(
    db: &HcomDb,
    name: &str,
    last_n: usize,
) -> Result<String, String> {
    render_instance_transcript_impl(
        db,
        name,
        &TranscriptRenderOpts {
            last_n,
            ..Default::default()
        },
    )
}

pub fn render_instance_transcript_with_options_no_retry(
    db: &HcomDb,
    name: &str,
    range: Option<&str>,
    last_n: usize,
    json_mode: bool,
    full_mode: bool,
    detailed: bool,
) -> Result<String, String> {
    render_instance_transcript_impl(
        db,
        name,
        &TranscriptRenderOpts {
            range,
            last_n,
            json_mode,
            full_mode,
            detailed,
            retry_codex: false,
        },
    )
}

pub fn render_instance_transcript_with_options(
    db: &HcomDb,
    name: &str,
    range: Option<&str>,
    last_n: usize,
    json_mode: bool,
    full_mode: bool,
    detailed: bool,
) -> Result<String, String> {
    render_instance_transcript_impl(
        db,
        name,
        &TranscriptRenderOpts {
            range,
            last_n,
            json_mode,
            full_mode,
            detailed,
            retry_codex: true,
        },
    )
}

fn render_instance_transcript_impl(
    db: &HcomDb,
    name: &str,
    opts: &TranscriptRenderOpts<'_>,
) -> Result<String, String> {
    let (instance_name, transcript_path, agent_type, session_id) =
        resolve_instance_transcript(db, name).ok_or_else(|| no_transcript_error(db, name))?;
    let (range_start, range_end) = if let Some(r) = opts.range {
        parse_range(r)
    } else {
        (None, None)
    };
    let effective_last = if range_start.is_some() {
        usize::MAX
    } else {
        opts.last_n
    };
    let exchanges = get_exchanges(
        &transcript_path,
        &agent_type,
        effective_last,
        opts.detailed,
        session_id.as_deref(),
        opts.retry_codex,
    )
    .map_err(|e| e.to_string())?;

    let filtered: Vec<&Exchange> = if let Some(start) = range_start {
        let end = range_end.unwrap_or(start);
        exchanges
            .iter()
            .filter(|e| e.position >= start && e.position <= end)
            .collect()
    } else {
        exchanges.iter().collect()
    };

    if opts.json_mode {
        let json_output: Vec<Value> = filtered
            .iter()
            .map(|ex| {
                let mut obj = json!({
                    "position": ex.position,
                    "user": ex.user,
                    "action": ex.action,
                    "files": ex.files,
                    "timestamp": ex.timestamp,
                });
                if opts.detailed {
                    obj["tools"] = json!(
                        ex.tools
                            .iter()
                            .map(|t| {
                                let mut tool = json!({
                                    "name": t.name,
                                    "is_error": t.is_error,
                                });
                                if let Some(ref f) = t.file {
                                    tool["file"] = json!(f);
                                }
                                if let Some(ref c) = t.command {
                                    tool["command"] = json!(c);
                                }
                                tool
                            })
                            .collect::<Vec<_>>()
                    );
                    obj["edits"] = json!(ex.edits);
                    obj["errors"] = json!(ex.errors);
                    obj["ended_on_error"] = json!(ex.ended_on_error);
                }
                obj
            })
            .collect();
        return serde_json::to_string_pretty(&json_output).map_err(|e| e.to_string());
    }

    if filtered.is_empty() {
        return Ok("No exchanges found".to_string());
    }

    let first_pos = filtered.first().map(|e| e.position).unwrap_or(1);
    let last_pos = filtered.last().map(|e| e.position).unwrap_or(1);
    let owned: Vec<Exchange> = filtered.into_iter().cloned().collect();
    let formatted = format_exchanges(&owned, &instance_name, opts.full_mode, opts.detailed);
    Ok(format!(
        "Recent conversation ({} exchanges, {}-{} of {}) - @{}:\n\n{}",
        owned.len(),
        first_pos,
        last_pos,
        exchanges.len(),
        instance_name,
        formatted
    ))
}

/// Resolve instance name to (name, transcript_path, agent_type, session_id).
fn resolve_instance_transcript(
    db: &HcomDb,
    name: &str,
) -> Option<(String, String, String, Option<String>)> {
    let name = crate::identity::resolve_display_name_or_stopped(db, name)
        .unwrap_or_else(|| name.to_string());

    // Direct match
    if let Some(path) = get_transcript_path(db, &name) {
        let (tool, sid) = db
            .conn()
            .query_row(
                "SELECT tool, session_id FROM instances WHERE name = ?",
                rusqlite::params![&name],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .unwrap_or_else(|_| (detect_agent_type(&path).to_string(), None));
        return Some((name, path, tool, sid));
    }

    // Prefix match
    if let Ok((matched_name, path, tool, sid)) = db.conn().query_row(
        "SELECT name, transcript_path, tool, session_id FROM instances WHERE name LIKE ? AND transcript_path IS NOT NULL AND transcript_path != '' LIMIT 1",
        rusqlite::params![format!("{}%", name)],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, Option<String>>(3)?)),
    ) {
        return Some((matched_name, path, tool, sid));
    }

    // Check stopped events (session_id from snapshot)
    if let Ok((path, sid)) = db.conn().query_row(
        "SELECT json_extract(data, '$.snapshot.transcript_path'), json_extract(data, '$.snapshot.session_id') FROM events WHERE type = 'life' AND instance = ? AND json_extract(data, '$.action') = 'stopped' ORDER BY id DESC LIMIT 1",
        rusqlite::params![&name],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
    ) {
        let agent = detect_agent_type(&path).to_string();
        return Some((name, path, agent, sid));
    }

    None
}

/// Parse range string "N-M" or "N".
fn parse_range(s: &str) -> (Option<usize>, Option<usize>) {
    if let Some(dash_pos) = s.find('-') {
        let start: Option<usize> = s[..dash_pos].parse().ok().filter(|&v: &usize| v >= 1);
        let end: Option<usize> = s[dash_pos + 1..].parse().ok().filter(|&v: &usize| v >= 1);
        // Validate start <= end
        if let (Some(s), Some(e)) = (start, end) {
            if s > e {
                eprintln!("Error: invalid range '{s}-{e}' (start must be <= end)");
                return (None, None);
            }
        }
        (start, end)
    } else {
        let pos: Option<usize> = s.parse().ok().filter(|&v: &usize| v >= 1);
        (pos, pos)
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::ToolUse;
    use crate::transcript::shared::finalize_action_text;
    use std::fs;

    fn test_db() -> HcomDb {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();
        std::mem::forget(dir);
        db
    }

    #[test]
    fn test_parse_range() {
        assert_eq!(parse_range("5"), (Some(5), Some(5)));
        assert_eq!(parse_range("3-10"), (Some(3), Some(10)));
        assert_eq!(parse_range("abc"), (None, None));
    }

    #[test]
    fn test_summarize_action() {
        let short = "Hello world";
        assert_eq!(summarize_action(short), "Hello world");

        let multi = "Line 1\nLine 2\nLine 3\nLine 4\nLine 5";
        let result = summarize_action(multi);
        assert!(result.contains("Line 1"));
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_detect_agent_type() {
        assert_eq!(
            detect_agent_type("/home/user/.claude/projects/x/transcript.jsonl"),
            "claude"
        );
        assert_eq!(
            detect_agent_type("/home/user/.gemini/tmp/session.json"),
            "gemini"
        );
        assert_eq!(
            detect_agent_type("/home/user/.codex/sessions/x/rollout.jsonl"),
            "codex"
        );
        assert_eq!(
            detect_agent_type("/home/user/.local/share/opencode/opencode.db"),
            "opencode"
        );
    }

    #[test]
    fn test_correlate_paths_to_hcom_uses_session_id_for_opencode() {
        let dir = tempfile::tempdir().unwrap();
        let db = HcomDb::open_raw(&dir.path().join("hcom.db")).unwrap();
        db.conn()
            .execute_batch(
                "CREATE TABLE instances (
                     name text,
                     project TEXT DEFAULT '',
                     transcript_path text,
                     session_id text
                 );
                 CREATE TABLE events (
                     id integer PRIMARY KEY,
                     type text,
                     instance text,
                     data text
                 );",
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, transcript_path, session_id) VALUES (?, ?, ?)",
                rusqlite::params!["luna", "/tmp/opencode.db", "ses_a"],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, transcript_path, session_id) VALUES (?, ?, ?)",
                rusqlite::params!["nova", "/tmp/opencode.db", "ses_b"],
            )
            .unwrap();

        let correlated = correlate_paths_to_hcom(
            &db,
            &[
                ("/tmp/opencode.db".to_string(), Some("ses_a".to_string())),
                ("/tmp/opencode.db".to_string(), Some("ses_b".to_string())),
                ("/tmp/file.jsonl".to_string(), None),
            ],
        );

        assert_eq!(
            correlated.get(&transcript_search_key("/tmp/opencode.db", Some("ses_a"))),
            Some(&"luna".to_string())
        );
        assert_eq!(
            correlated.get(&transcript_search_key("/tmp/opencode.db", Some("ses_b"))),
            Some(&"nova".to_string())
        );
    }

    #[test]
    fn test_transcript_display_for_tool_only_and_error_turns() {
        let tools = vec![ToolUse {
            name: "Bash".to_string(),
            is_error: false,
            file: None,
            command: Some("pwd".to_string()),
        }];
        assert_eq!(
            finalize_action_text("", &tools, &[], false),
            "(tool-only turn: Bash)"
        );

        let errors = vec![json!({"tool": "Bash", "content": "Exit code: 1"})];
        assert_eq!(
            finalize_action_text("", &tools, &errors, true),
            "(turn ended in error after using Bash)"
        );
    }

    #[test]
    fn test_render_instance_transcript_with_options_range_matches_cli_shape() {
        let dir = tempfile::tempdir().unwrap();
        let transcript_path = dir.path().join("rollout.jsonl");
        let db = test_db();
        let now = crate::shared::time::now_epoch_f64();
        let lines = [
            json!({
                "type": "response_item",
                "timestamp": "2026-03-27T10:00:00Z",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "first user"}]
                }
            }),
            json!({
                "type": "response_item",
                "timestamp": "2026-03-27T10:00:01Z",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "first answer"}]
                }
            }),
            json!({
                "type": "response_item",
                "timestamp": "2026-03-27T10:01:00Z",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "second user"}]
                }
            }),
            json!({
                "type": "response_item",
                "timestamp": "2026-03-27T10:01:01Z",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "second answer"}]
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

        let mut data = serde_json::Map::new();
        data.insert("created_at".into(), json!(now));
        data.insert("tool".into(), json!("codex"));
        data.insert(
            "transcript_path".into(),
            json!(transcript_path.to_string_lossy().to_string()),
        );
        db.save_instance_named("luna", &data).unwrap();

        let rendered = render_instance_transcript_with_options(
            &db,
            "luna",
            Some("2"),
            10,
            false,
            false,
            false,
        )
        .unwrap();

        assert!(rendered.contains("Recent conversation (1 exchanges, 2-2 of 2) - @luna:"));
        assert!(rendered.contains("second user"));
        assert!(rendered.contains("second answer"));
        assert!(!rendered.contains("first user"));
    }

    #[test]
    fn test_render_instance_transcript_with_options_json_contract() {
        let dir = tempfile::tempdir().unwrap();
        let transcript_path = dir.path().join("rollout.jsonl");
        let db = test_db();
        let now = crate::shared::time::now_epoch_f64();
        let lines = [
            json!({
                "type": "response_item",
                "timestamp": "2026-03-27T10:00:00Z",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "user prompt"}]
                }
            }),
            json!({
                "type": "response_item",
                "timestamp": "2026-03-27T10:00:01Z",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "assistant answer"}]
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

        let mut data = serde_json::Map::new();
        data.insert("created_at".into(), json!(now));
        data.insert("tool".into(), json!("codex"));
        data.insert(
            "transcript_path".into(),
            json!(transcript_path.to_string_lossy().to_string()),
        );
        db.save_instance_named("luna", &data).unwrap();

        let rendered =
            render_instance_transcript_with_options(&db, "luna", None, 10, true, false, false)
                .unwrap();
        let parsed: Value = serde_json::from_str(&rendered).unwrap();
        let first = parsed.as_array().unwrap().first().unwrap();

        assert_eq!(first["position"], 1);
        assert_eq!(first["user"], "user prompt");
        assert_eq!(first["action"], "assistant answer");
    }

    #[test]
    fn test_finalize_action_text_uses_final_error_state_only() {
        let tools = vec![
            ToolUse {
                name: "Edit".to_string(),
                is_error: true,
                file: Some("a.rs".to_string()),
                command: None,
            },
            ToolUse {
                name: "Edit".to_string(),
                is_error: false,
                file: Some("a.rs".to_string()),
                command: None,
            },
        ];
        let errors = vec![json!({"tool": "Edit", "content": "old failure"})];
        assert_eq!(
            finalize_action_text("", &tools, &errors, false),
            "(tool-only turn: Edit)"
        );
    }

    // ── Clap parse tests ─────────────────────────────────────────────

    use clap::Parser;

    #[test]
    fn test_transcript_view_basic() {
        let args = TranscriptArgs::try_parse_from(["transcript", "peso"]).unwrap();
        assert!(args.subcmd.is_none());
        assert_eq!(args.name.as_deref(), Some("peso"));
        assert!(!args.json);
    }

    #[test]
    fn test_transcript_view_with_flags() {
        let args = TranscriptArgs::try_parse_from([
            "transcript",
            "@peso",
            "--json",
            "--full",
            "--last",
            "5",
        ])
        .unwrap();
        assert_eq!(args.name.as_deref(), Some("@peso"));
        assert!(args.json);
        assert!(args.full);
        assert_eq!(args.last, Some(5));
    }

    #[test]
    fn test_transcript_view_range() {
        let args = TranscriptArgs::try_parse_from(["transcript", "peso", "3-10"]).unwrap();
        assert_eq!(args.name.as_deref(), Some("peso"));
        assert_eq!(args.range_positional.as_deref(), Some("3-10"));
    }

    #[test]
    fn test_transcript_search() {
        let args = TranscriptArgs::try_parse_from([
            "transcript",
            "search",
            "error",
            "--live",
            "--limit",
            "50",
        ])
        .unwrap();
        match args.subcmd {
            Some(TranscriptSubcmd::Search(ref s)) => {
                assert_eq!(s.pattern, "error");
                assert!(s.live);
                assert_eq!(s.limit, 50);
            }
            _ => panic!("expected Search subcommand"),
        }
    }

    #[test]
    fn test_transcript_timeline() {
        let args =
            TranscriptArgs::try_parse_from(["transcript", "timeline", "--json", "--last", "3"])
                .unwrap();
        match args.subcmd {
            Some(TranscriptSubcmd::Timeline(ref t)) => {
                assert!(t.json);
                assert_eq!(t.last, Some(3));
            }
            _ => panic!("expected Timeline subcommand"),
        }
    }

    #[test]
    fn test_transcript_rejects_bogus() {
        assert!(TranscriptArgs::try_parse_from(["transcript", "--bogus"]).is_err());
    }
}
