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
use crate::tool::Tool;
use crate::transcript::{
    self, Exchange, ReadOptions, format_transcript_exchanges, summarize_exchange,
};

fn run_search_tool(program: &str, args: &[&str]) -> Result<Option<std::process::Output>, String> {
    match std::process::Command::new(program).args(args).output() {
        Ok(output) if output.status.success() => Ok(Some(output)),
        Ok(output) if output.status.code() == Some(1) => Ok(None),
        Ok(output) => {
            let detail = String::from_utf8_lossy(&output.stderr);
            Err(format!(
                "{program} failed{}",
                if detail.trim().is_empty() {
                    format!(" with {}", output.status)
                } else {
                    format!(": {}", detail.trim())
                }
            ))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(format!(
            "required search tool `{program}` was not found on PATH"
        )),
        Err(err) => Err(format!("could not run `{program}`: {err}")),
    }
}

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
    #[arg(value_name = "range")]
    pub range_positional: Option<String>,

    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Full output (no streamlining)
    #[arg(long)]
    pub full: bool,
    /// Show tool inputs/outputs, file edits, and errors
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
    /// Only currently live agents
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
    /// Max matching exchanges shown (default: 20)
    #[arg(long, default_value = "20")]
    pub limit: usize,
    /// Filter by exact agent type (canonical name or declared alias)
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
    /// Number of exchanges to show, newest first (default: 10)
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

/// Snippet width (bytes) for transcript search matches, centered on the hit.
const SEARCH_SNIPPET_WIDTH: usize = 160;

/// Build a snippet of ~`width` bytes centered on a match.
///
/// Transcript lines are whole JSON message objects (thousands of bytes), so a
/// start-anchored truncation only ever shows leading metadata (`parentUuid`…),
/// never the match. `col` is ripgrep's 1-based byte column of the match start;
/// we window around it so the matched text is actually visible, with `…`
/// markers when content is elided on either side. All slice points are snapped
/// to UTF-8 char boundaries.
fn centered_snippet(line_text: &str, col: usize, width: usize) -> String {
    if line_text.len() <= width {
        return line_text.to_string();
    }
    let match_start = col.saturating_sub(1).min(line_text.len());
    let half = width / 2;
    let mut start = match_start.saturating_sub(half);
    while start > 0 && !line_text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (start + width).min(line_text.len());
    while end < line_text.len() && !line_text.is_char_boundary(end) {
        end += 1;
    }
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(line_text[start..end].trim());
    if end < line_text.len() {
        out.push('…');
    }
    out
}

/// Parse one `rg -n --column` output line (`LINE:COL:TEXT`) into
/// `(line_number, centered_snippet)`.
fn parse_match_line(raw: &str) -> (usize, String) {
    let mut parts = raw.splitn(3, ':');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(line), Some(col), Some(text)) => (
            line.parse().unwrap_or(0),
            centered_snippet(text, col.parse().unwrap_or(1), SEARCH_SNIPPET_WIDTH),
        ),
        _ => (0, centered_snippet(raw, 1, SEARCH_SNIPPET_WIDTH)),
    }
}

// ── Transcript Path Discovery ────────────────────────────────────────────

/// Detect canonical agent type from transcript path.
pub(crate) fn detect_agent_type(path: &str) -> &'static str {
    transcript::agent_name_from_path(path)
}

fn transcript_search_key(path: &str, session_id: Option<&str>) -> String {
    format!("{path}\u{0}{}", session_id.unwrap_or(""))
}

/// Attribute a `--all` disk match to a canonical tool.
///
/// Content detection wins when it lands on a selected tool: it resolves every
/// signatured format and the one shared root (gemini/antigravity under
/// `~/.gemini`). Otherwise the file is attributed by provenance — the search
/// root it was found under — which is what classifies unsignatured sessions such
/// as pi's bare `<uuid>.jsonl` reached via a custom `PI_CODING_AGENT_SESSION_DIR`.
/// Provenance is only trusted when exactly one selected root owns the path, so
/// the shared gemini/antigravity root never guesses.
fn attribute_disk_match(
    file_path: &str,
    selected: &[Tool],
    root_owners: &[(PathBuf, Tool)],
) -> Option<Tool> {
    if let Some(detected) = transcript::detect_tool_from_path(file_path)
        && selected.contains(&detected)
    {
        return Some(detected);
    }
    let path = Path::new(file_path);
    let mut owner: Option<Tool> = None;
    for (root, tool) in root_owners {
        if selected.contains(tool) && path.starts_with(root) {
            match owner {
                None => owner = Some(*tool),
                Some(existing) if existing == *tool => {}
                Some(_) => return None, // ambiguous provenance — do not guess
            }
        }
    }
    owner
}

/// "Recent conversation (2 exchanges, 3-4 of 4) - @name:". Positions count
/// from the session's first exchange, and `--last` loads only the newest ones,
/// so the total is the newest exchange's position, not how many were loaded.
fn conversation_header(shown: &[&Exchange], loaded: &[Exchange], instance_name: &str) -> String {
    let first = shown.first().map_or(0, |e| e.position);
    let last = shown.last().map_or(0, |e| e.position);
    let total = loaded.iter().map(|e| e.position).max().unwrap_or(0);
    let count = shown.len();
    let noun = if count == 1 { "exchange" } else { "exchanges" };
    let span = if first == last {
        first.to_string()
    } else {
        format!("{first}-{last}")
    };
    format!("Recent conversation ({count} {noun}, {span} of {total}) - @{instance_name}:")
}

/// Explain an empty range result against the exchanges that do exist.
fn range_miss_message(exchanges: &[Exchange], range: Option<ExchangeRange>) -> Option<String> {
    let (start, end) = range?;
    let last = exchanges.iter().map(|e| e.position).max()?;
    let asked = match end {
        Some(end) if end == start => start.to_string(),
        Some(end) => format!("{start}-{end}"),
        None => format!("{start}-"),
    };
    Some(format!(
        "No exchanges in range {asked}; this transcript has 1-{last} (e.g. --last 5 or {}-{last})",
        last.saturating_sub(4).max(1)
    ))
}

/// Build an appropriate error message when transcript resolution fails.
/// Uses resolve_display_name_or_stopped (which handles exact base and tag-name
/// resolution) to check if the instance exists without a transcript.
fn no_transcript_error(
    db: &HcomDb,
    name: &str,
    display_name: &str,
    device: Option<&str>,
) -> String {
    if let Some(resolved) = crate::identity::resolve_display_name_or_stopped(db, name) {
        let display_name = if display_name.is_empty() {
            &resolved
        } else {
            display_name
        };
        let command = match device {
            Some(device) => format!(
                "hcom events --remote-fetch --device {device} --participant {resolved} --type message"
            ),
            None => format!("hcom events --participant {resolved} --type message"),
        };
        format!(
            "No model transcript is registered for {display_name}.\nView transport messages with: {command}"
        )
    } else {
        crate::identity::describe_missing_agent(db, name)
    }
}

/// Find a device-suffixed instance name ("dami:KEZE") matching a bare base
/// name ("dami") that has no exact/tag/stopped match of its own. Relay pull
/// always namespaces remote instances with a device suffix (see
/// `relay::add_device_suffix`), so a bare name typed by the user never
/// matches those rows directly — only this prefix lookup does. Without it,
/// callers fall through to a plain-name DB lookup, which still succeeds via
/// a looser `LIKE` prefix match and returns the remote device's transcript
/// path as if it were a local file.
fn resolve_remote_instance_name(db: &HcomDb, base_name: &str) -> Option<String> {
    db.conn()
        .query_row(
            "SELECT name FROM instances WHERE name LIKE ?1 ESCAPE '\\' ORDER BY status_time DESC LIMIT 1",
            rusqlite::params![format!(
                "{}:____",
                base_name.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
            )],
            |row| row.get::<_, String>(0),
        )
        .ok()
}

/// Get exchanges from a transcript file using the shared transcript module.
pub(crate) fn get_exchanges(
    path: &str,
    agent: &str,
    last: usize,
    detailed: bool,
    session_id: Option<&str>,
    retry_codex: bool,
) -> Result<Vec<Exchange>, String> {
    get_transcript(path, agent, last, detailed, session_id, retry_codex)
        .map(|transcript| transcript.exchanges)
}

pub(crate) fn get_transcript(
    path: &str,
    agent: &str,
    last: usize,
    detailed: bool,
    session_id: Option<&str>,
    retry_codex: bool,
) -> Result<transcript::Transcript, String> {
    let backend = transcript::backend_from_agent_or_path(agent, path)?;
    let opts = ReadOptions {
        last,
        detailed,
        session_id: session_id.map(|s| s.to_string()),
        allow_codex_retry: retry_codex,
    };
    transcript::read(Path::new(path), backend, &opts)
}

// ── Tracked transcripts ──────────────────────────────────────────────────

/// One hcom-tracked transcript: a live agent's, or a stopped agent's snapshot.
struct TrackedTranscript {
    name: String,
    path: String,
    tool: String,
    session_id: Option<String>,
    /// When the agent stopped; `None` while it is live.
    stopped_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Live agents first, then stopped snapshots newest first, one entry per agent
/// name: the session `hcom transcript <name>` opens, so every reference printed
/// for a result leads back to it. Empty paths are skipped so a transcript-less
/// snapshot can't shadow a real one.
fn tracked_transcripts(db: &HcomDb, live_only: bool) -> Vec<TrackedTranscript> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |t: TrackedTranscript| {
        if seen.insert(t.name.clone()) {
            out.push(t);
        }
    };

    if let Ok(mut stmt) = db.conn().prepare(
        "SELECT name, transcript_path, tool, session_id FROM instances
         WHERE transcript_path IS NOT NULL AND transcript_path != ''",
    ) && let Ok(rows) = stmt.query_map([], |row| {
        Ok(TrackedTranscript {
            name: row.get(0)?,
            path: row.get(1)?,
            tool: row.get(2)?,
            session_id: row.get(3)?,
            stopped_at: None,
        })
    }) {
        rows.flatten().for_each(&mut push);
    }

    if live_only {
        return out;
    }

    if let Ok(mut stmt) = db.conn().prepare(
        "SELECT instance,
                json_extract(data, '$.snapshot.transcript_path') AS tp,
                json_extract(data, '$.snapshot.tool'),
                json_extract(data, '$.snapshot.session_id'),
                timestamp
         FROM events WHERE type = 'life'
         AND json_extract(data, '$.action') = 'stopped'
         AND tp IS NOT NULL AND tp != ''
         ORDER BY id DESC",
    ) && let Ok(rows) = stmt.query_map([], |row| {
        let path: String = row.get(1)?;
        let tool = row
            .get::<_, Option<String>>(2)?
            .unwrap_or_else(|| detect_agent_type(&path).to_string());
        Ok(TrackedTranscript {
            name: row.get(0)?,
            path,
            tool,
            session_id: row.get(3)?,
            stopped_at: parse_timestamp(&row.get::<_, String>(4)?),
        })
    }) {
        rows.flatten().for_each(&mut push);
    }

    out
}

/// Parse a transcript or event timestamp (RFC 3339, or epoch seconds/millis).
fn parse_timestamp(ts: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        return Some(dt.with_timezone(&chrono::Utc));
    }
    let n: f64 = ts.parse().ok()?;
    let millis = if n > 1e12 { n } else { n * 1000.0 };
    chrono::DateTime::from_timestamp_millis(millis as i64)
}

/// Local "HH:MM" for today, "Mon D HH:MM" otherwise.
fn format_local_time(ts: &str) -> String {
    let Some(dt) = parse_timestamp(ts) else {
        return "??:??".to_string();
    };
    let local = dt.with_timezone(&chrono::Local);
    let fmt = if local.date_naive() == chrono::Local::now().date_naive() {
        "%H:%M"
    } else {
        "%b %-d %H:%M"
    };
    local.format(fmt).to_string()
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
    ) && let Ok(rows) = stmt.query_map([], |row| {
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

    // 2. Check stopped events for paths not yet matched
    if let Ok(mut stmt) = conn.prepare(
        "SELECT instance,
                json_extract(data, '$.snapshot.transcript_path') as tp,
                json_extract(data, '$.snapshot.session_id') as session_id \
         FROM events WHERE type = 'life' \
         AND json_extract(data, '$.action') = 'stopped' \
         AND json_extract(data, '$.snapshot.transcript_path') IS NOT NULL \
         ORDER BY id DESC",
    ) && let Ok(rows) = stmt.query_map([], |row| {
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

    result
}

/// A search hit in one exchange of a tracked transcript.
struct SearchHit<'a> {
    source: &'a TrackedTranscript,
    agent: &'static str,
    position: usize,
    timestamp: String,
    /// Where the match is: "user", "assistant", or "tool" (commands/files).
    field: &'static str,
    snippet: String,
}

/// First match in an exchange, checked in reading order: prompt, response,
/// then tool calls (commands, files, captured output, errors). Tool output is
/// the parser's capped capture, so text deep in a long output is not seen.
fn match_exchange(ex: &Exchange, re: &regex::Regex) -> Option<(&'static str, String)> {
    let tool_text = ex
        .tools
        .iter()
        .flat_map(|t| t.command.iter().chain(t.file.iter()).chain(t.output.iter()))
        .chain(ex.files.iter())
        .map(String::as_str)
        .chain(
            ex.errors
                .iter()
                .filter_map(|e| e.get("content").and_then(Value::as_str)),
        )
        .collect::<Vec<_>>()
        .join("\n");
    [
        ("user", ex.user.as_str()),
        ("assistant", ex.action.as_str()),
        ("tool", tool_text.as_str()),
    ]
    .into_iter()
    .find_map(|(field, text)| {
        let m = re.find(text)?;
        let snippet = centered_snippet(text, m.start() + 1, SEARCH_SNIPPET_WIDTH);
        Some((
            field,
            snippet.split_whitespace().collect::<Vec<_>>().join(" "),
        ))
    })
}

/// Every exchange of one transcript that matches `re`.
fn search_transcript<'a>(
    source: &'a TrackedTranscript,
    tool: Tool,
    re: &regex::Regex,
) -> Vec<SearchHit<'a>> {
    let Ok(exchanges) = get_exchanges(
        &source.path,
        &source.tool,
        usize::MAX,
        true,
        source.session_id.as_deref(),
        false,
    ) else {
        return Vec::new();
    };
    exchanges
        .iter()
        .filter_map(|ex| {
            let (field, snippet) = match_exchange(ex, re)?;
            Some(SearchHit {
                source,
                agent: tool.as_str(),
                position: ex.position,
                timestamp: ex.timestamp.clone(),
                field,
                snippet,
            })
        })
        .collect()
}

/// Transcript files that contain `pattern` on disk, or `None` when that can't
/// be decided from the raw bytes. Files hold JSON-escaped text, so most regex
/// constructs (`.`, classes, anchors, `\s`) match the decoded text differently
/// than the escaped bytes. Only a plain literal free of characters JSON may
/// escape is checked; everything else (and a missing `rg`) parses every
/// transcript.
fn prefilter_files(
    pattern: &str,
    files: &[&str],
) -> Result<Option<std::collections::HashSet<String>>, String> {
    let plain_literal = !pattern.is_empty()
        && pattern.chars().all(|c| c.is_ascii_graphic() || c == ' ')
        && !pattern.contains(|c| "\\\".^$*+?()[]{}|".contains(c));
    if files.is_empty() || !plain_literal {
        return Ok(None);
    }
    let mut args = vec!["-l", "-F", "-e", pattern, "--"];
    args.extend_from_slice(files);
    match run_search_tool("rg", &args) {
        Ok(Some(out)) => Ok(Some(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::to_string)
                .collect(),
        )),
        Ok(None) => Ok(Some(Default::default())),
        Err(err) if err.contains("was not found on PATH") => Ok(None),
        Err(err) => Err(err),
    }
}

/// Search across transcripts: `hcom transcript search "pattern" [--live] [--all] [--limit N] [--exclude-self]`
fn cmd_transcript_search(
    db: &HcomDb,
    args: &TranscriptSearchArgs,
    ctx: Option<&CommandContext>,
) -> i32 {
    let agent_filter = match args.agent.as_deref() {
        Some(value) => match transcript::parse_tool_filter(value) {
            Ok(tool) => Some(tool),
            Err(error) => {
                eprintln!("Error: {error}");
                return 1;
            }
        },
        None => None,
    };

    if args.all {
        return cmd_transcript_search_all(db, args, agent_filter);
    }

    let re = match regex::Regex::new(&args.pattern) {
        Ok(re) => re,
        Err(err) => {
            eprintln!("Error: invalid pattern: {err}");
            return 1;
        }
    };

    let self_name = if args.exclude_self {
        ctx.and_then(|c| c.identity.as_ref())
            .filter(|id| matches!(id.kind, crate::shared::SenderKind::Instance))
            .map(|id| id.name.clone())
    } else {
        None
    };

    // (source, tool, backend) for every transcript in scope.
    let sources: Vec<TrackedTranscript> = tracked_transcripts(db, args.live)
        .into_iter()
        .filter(|t| self_name.as_deref() != Some(t.name.as_str()))
        .filter(|t| Path::new(&t.path).exists())
        .collect();
    let resolved: Vec<(&TrackedTranscript, Tool, transcript::TranscriptBackend)> = sources
        .iter()
        .filter_map(|t| {
            let tool = transcript::tool_from_agent_or_path(&t.tool, &t.path).ok()?;
            let backend = transcript::backend_for_tool(tool)?;
            Some((t, tool, backend))
        })
        .filter(|(_, tool, _)| agent_filter.is_none_or(|f| f == *tool))
        .collect();

    // A shared database holds every session, so a raw byte match says nothing
    // about this agent's; only per-session files can be prefiltered.
    let files: Vec<&str> = resolved
        .iter()
        .filter(|(_, _, backend)| !backend.is_shared_database())
        .map(|(t, _, _)| t.path.as_str())
        .collect();
    let candidates = match prefilter_files(&args.pattern, &files) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("Error: {err}");
            return 1;
        }
    };

    let to_parse: Vec<_> = resolved
        .iter()
        .filter(|(source, _, backend)| {
            backend.is_shared_database()
                || candidates.as_ref().is_none_or(|c| c.contains(&source.path))
        })
        .collect();
    // Parsing dominates (transcripts run to megabytes), so spread it over cores.
    let workers = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(to_parse.len().max(1));
    let mut hits: Vec<SearchHit> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|w| {
                let (to_parse, re) = (&to_parse, &re);
                scope.spawn(move || {
                    to_parse
                        .iter()
                        .skip(w)
                        .step_by(workers)
                        .flat_map(|(source, tool, _)| search_transcript(source, *tool, re))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });

    let total = hits.len();
    hits.sort_by_key(|h| std::cmp::Reverse(parse_timestamp(&h.timestamp)));
    hits.truncate(args.limit);

    let scope = if args.live { "live" } else { "hcom" };
    if args.json {
        let results: Vec<Value> = hits
            .iter()
            .map(|h| {
                json!({
                    "hcom_name": h.source.name,
                    "agent": h.agent,
                    "position": h.position,
                    "timestamp": h.timestamp,
                    "field": h.field,
                    "text": h.snippet,
                    "path": h.source.path,
                    "session_id": h.source.session_id,
                })
            })
            .collect();
        println!(
            "{}",
            json!({"count": results.len(), "total": total, "results": results, "scope": scope})
        );
        return 0;
    }

    if hits.is_empty() {
        println!("No matches for \"{}\"", args.pattern);
        return 0;
    }
    let scope_label = if args.live {
        "live agents"
    } else {
        "hcom-tracked"
    };
    let noun = if total == 1 { "exchange" } else { "exchanges" };
    if hits.len() < total {
        println!(
            "{} of {total} matching {noun} ({scope_label}, newest first; --limit for more):\n",
            hits.len()
        );
    } else {
        println!("{total} matching {noun} ({scope_label}, newest first):\n");
    }
    for h in &hits {
        let flag = if h.field == "tool" {
            "--detailed"
        } else {
            "--full"
        };
        println!(
            "[{}:{} #{}] {} {}",
            h.agent,
            h.source.name,
            h.position,
            format_local_time(&h.timestamp),
            h.field
        );
        println!("    {}", h.snippet);
        println!(
            "    hcom transcript {} {} {flag}\n",
            h.source.name, h.position
        );
    }
    0
}

/// `transcript search --all`: raw ripgrep over every transcript on disk,
/// including sessions hcom never tracked.
fn cmd_transcript_search_all(
    db: &HcomDb,
    args: &TranscriptSearchArgs,
    agent_filter: Option<Tool>,
) -> i32 {
    let pattern = args.pattern.as_str();
    let limit = args.limit;
    let selected_tools = agent_filter
        .map(|tool| vec![tool])
        .unwrap_or_else(transcript::transcript_tools);
    let mut search_dirs: Vec<PathBuf> = Vec::new();
    // Remember which tool each search root belongs to so matches found under
    // an override root with no content signature (e.g. pi's bare
    // `<uuid>.jsonl` under a custom PI_CODING_AGENT_SESSION_DIR) can still be
    // attributed. A path can map to more than one tool — gemini and
    // antigravity share `~/.gemini` — which `attribute_disk_match` treats as
    // ambiguous and defers to content detection.
    let mut root_owners: Vec<(PathBuf, Tool)> = Vec::new();
    for tool in &selected_tools {
        for path in transcript::disk_search_roots(*tool) {
            if path.exists() {
                if !search_dirs.contains(&path) {
                    search_dirs.push(path.clone());
                }
                root_owners.push((path, *tool));
            }
        }
    }
    let database_sources: Vec<(Tool, PathBuf)> = selected_tools
        .iter()
        .filter_map(|tool| transcript::database_search_path(*tool).map(|path| (*tool, path)))
        .collect();

    if search_dirs.is_empty() && database_sources.is_empty() {
        println!("No transcript directories or databases found on disk.");
        return 0;
    }

    // Phase 1: matching files with per-file matching-line counts.
    // Avoid invoking rg without a path when only SQLite sources exist; that
    // would make it read stdin and potentially block an interactive command.
    let file_counts: Vec<(String, u64)> = if search_dirs.is_empty() {
        Vec::new()
    } else {
        let mut cmd = std::process::Command::new("rg");
        cmd.args([
            "-c", "--glob", "*.jsonl", "--glob", "*.json", "-e", pattern, "--",
        ])
        .args(&search_dirs);
        match cmd.output() {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|line| {
                    let (path, count) = line.rsplit_once(':')?;
                    Some((path.to_string(), count.parse().ok()?))
                })
                .collect(),
            Ok(out) if out.status.code() == Some(1) => Vec::new(),
            Ok(out) => {
                eprintln!(
                    "Error: ripgrep failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
                return 1;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("Error: transcript search --all requires `rg` (ripgrep) on PATH");
                return 1;
            }
            Err(err) => {
                eprintln!("Error: could not run `rg`: {err}");
                return 1;
            }
        }
    };

    let mut database_matches = Vec::new();
    for (tool, db_path) in &database_sources {
        if database_matches.len() >= limit {
            break;
        }
        match transcript::search_database_sessions(
            *tool,
            db_path,
            pattern,
            limit - database_matches.len(),
        ) {
            Ok(matches) => database_matches.extend(matches),
            Err(err) => {
                eprintln!("Error: {err}");
                return 1;
            }
        }
    }

    if file_counts.is_empty() && database_matches.is_empty() {
        if args.json {
            println!("{}", json!({"count": 0, "results": [], "scope": "all"}));
        } else {
            println!("No matches for \"{pattern}\"");
        }
        return 0;
    }

    // Correlate transcript paths/session IDs to hcom names via DB.
    let mut targets: Vec<(String, Option<String>)> = file_counts
        .iter()
        .map(|(path, _)| (path.clone(), None))
        .collect();
    targets.extend(
        database_matches
            .iter()
            .filter_map(|m| m.session_id.clone().map(|sid| (m.path.clone(), Some(sid)))),
    );
    let path_to_hcom = correlate_paths_to_hcom(db, &targets);

    // Phase 2: first matching line of each file, for the snippet.
    let mut results = Vec::new();
    for (file_path, match_count) in &file_counts {
        if results.len() >= limit {
            break;
        }
        let Some(detected_tool) = attribute_disk_match(file_path, &selected_tools, &root_owners)
        else {
            continue;
        };
        let out = match run_search_tool(
            "rg",
            &["-n", "--column", "-m", "1", "-e", pattern, "--", file_path],
        ) {
            Ok(Some(out)) => out,
            Ok(None) => continue,
            Err(err) => {
                eprintln!("Error: {err}");
                return 1;
            }
        };
        let stdout = String::from_utf8_lossy(&out.stdout);
        let Some(first) = stdout.lines().next() else {
            continue;
        };
        let (line_num, snippet) = parse_match_line(first);
        let hcom_name = path_to_hcom.get(&transcript_search_key(file_path, None));
        results.push(json!({
            "hcom_name": hcom_name,
            "agent": detected_tool.as_str(),
            "path": file_path,
            "line": line_num,
            "text": snippet,
            "matches": match_count,
        }));
    }

    for database_match in &database_matches {
        if results.len() >= limit {
            break;
        }
        let hcom_name = path_to_hcom.get(&transcript_search_key(
            &database_match.path,
            database_match.session_id.as_deref(),
        ));
        results.push(json!({
            "hcom_name": hcom_name,
            "agent": database_match.agent,
            "path": database_match.path,
            "line": database_match.line,
            "text": database_match.text,
            "matches": database_match.matches,
            "session_id": database_match.session_id,
            "label": database_match.label,
        }));
    }

    if args.json {
        println!(
            "{}",
            json!({"count": results.len(), "results": results, "scope": "all"})
        );
        return 0;
    }
    if results.is_empty() {
        println!("No matches for \"{pattern}\"");
        return 0;
    }
    let total = file_counts.len() + database_matches.len();
    if results.len() < total {
        println!(
            "Matches in {} of {total} transcripts (all on disk; --limit for more):",
            results.len()
        );
    } else {
        println!("Matches in {total} transcripts (all on disk):");
    }
    for r in &results {
        let path = r["path"].as_str().unwrap_or("");
        let agent = r["agent"].as_str().unwrap_or("?");
        let line = r["line"].as_u64().unwrap_or(0);
        let matches = r["matches"].as_u64().unwrap_or(0);
        let snippet = r["text"].as_str().unwrap_or("");
        let label = r["label"].as_str().unwrap_or("");
        let session_id = r["session_id"].as_str().unwrap_or("");
        let short_path = path
            .rsplit('/')
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
        let noun = if matches == 1 { "line" } else { "lines" };
        println!("  [{agent}]{name_part} .../{short_path}:{line}  ({matches} matching {noun})");
        if !label.is_empty() || !session_id.is_empty() {
            let details: Vec<&str> = [label, session_id]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect();
            println!("    {}", details.join(" | "));
        }
        if !snippet.is_empty() {
            println!("    {snippet}");
        }
    }
    0
}

/// Timeline: `hcom transcript timeline [--last N] [--full] [--json]`
fn cmd_transcript_timeline(db: &HcomDb, args: &TranscriptTimelineArgs) -> i32 {
    let json_mode = args.json;
    let full_mode = args.full;
    let last_n = args.last.unwrap_or(10);

    // Sources come live first, then stopped newest first. A stopped transcript
    // has nothing newer than its stop time, so once `last_n` entries are all
    // newer than the next stop time, no remaining source can contribute.
    let mut entries: Vec<(Option<chrono::DateTime<chrono::Utc>>, Value)> = Vec::new();
    for source in tracked_transcripts(db, false) {
        if let Some(stopped_at) = source.stopped_at
            && entries.len() >= last_n
        {
            entries.sort_by_key(|(ts, _)| std::cmp::Reverse(*ts));
            entries.truncate(last_n);
            if entries
                .last()
                .is_some_and(|(ts, _)| *ts >= Some(stopped_at))
            {
                break;
            }
        }
        let Ok(exchanges) = get_exchanges(
            &source.path,
            &source.tool,
            last_n,
            args.detailed,
            source.session_id.as_deref(),
            true,
        ) else {
            continue;
        };
        for ex in exchanges {
            entries.push((
                parse_timestamp(&ex.timestamp),
                json!({
                    "instance": source.name,
                    "position": ex.position,
                    "user": ex.user,
                    "action": if full_mode { ex.action.clone() } else { summarize_exchange(&ex) },
                    "timestamp": ex.timestamp,
                    "files": ex.files,
                }),
            ));
        }
    }
    entries.sort_by_key(|(ts, _)| std::cmp::Reverse(*ts));
    entries.truncate(last_n);
    let all_entries: Vec<Value> = entries.into_iter().map(|(_, v)| v).collect();

    if json_mode {
        println!(
            "{}",
            serde_json::to_string(&all_entries).unwrap_or_default()
        );
        return 0;
    }

    if all_entries.is_empty() {
        println!("No transcript entries found");
        return 0;
    }

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

        let user_display = if user.len() > 80 {
            format!("{}...", truncate_str(user, 77))
        } else {
            user.to_string()
        };

        println!("[{}] \"{user_display}\"", format_local_time(ts));

        if full_mode {
            for action_line in action.lines().take(10) {
                println!("  {action_line}");
            }
            let line_count = action.lines().count();
            if line_count > 10 {
                println!("  ... (+{} lines)", line_count - 10);
            }
        } else {
            let action_display = if action.len() > 100 {
                format!("{}...", truncate_str(action, 97))
            } else {
                action.to_string()
            };
            println!("  → {action_display}");
        }

        if let Some(file_arr) = files {
            let file_strs: Vec<&str> = file_arr.iter().take(5).filter_map(|v| v.as_str()).collect();
            if !file_strs.is_empty() {
                println!("  Files: {}", file_strs.join(", "));
            }
        }

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
            .or_else(|| resolve_remote_instance_name(db, stripped))
            .unwrap_or_else(|| stripped.to_string());
        if let Some((base_name, device)) = crate::relay::control::split_device_suffix(&resolved) {
            return crate::relay::control::dispatch_remote_and_print(
                db,
                device,
                Some(&resolved),
                crate::relay::control::rpc_action::TRANSCRIPT,
                &json!({
                    "target": base_name,
                    "display_target": resolved,
                    "origin_device": device,
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

    // A lone numeric positional is a range for the caller's own transcript.
    let mut target = None;
    let mut range = args.range_flag.clone();
    if let Some(ref name) = args.name {
        let stripped = name.strip_prefix('@').unwrap_or(name);
        if stripped.chars().all(|c| c.is_ascii_digit() || c == '-')
            && stripped.chars().any(|c| c.is_ascii_digit())
        {
            range.get_or_insert_with(|| stripped.to_string());
        } else {
            target = Some(stripped.to_string());
        }
    }
    if range.is_none() {
        range = args.range_positional.clone();
    }

    let Some(name) = target.or_else(|| {
        ctx.and_then(|c| c.identity.as_ref())
            .map(|id| id.name.clone())
    }) else {
        return crate::commands::help::usage_error(
            "transcript",
            "hcom transcript needs an agent name",
        );
    };

    let opts = TranscriptRenderOpts {
        range: range.as_deref(),
        last_n,
        json_mode,
        full_mode,
        detailed,
        retry_codex: true,
    };
    match render_instance_transcript_impl(db, &name, &opts, &name, None) {
        Ok(out) => {
            println!("{out}");
            0
        }
        Err(err) => {
            eprintln!("Error: {err}");
            1
        }
    }
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
        name,
        None,
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
        name,
        None,
    )
}

/// Render a remote transcript while retaining the caller's device-qualified
/// name in diagnostics.
pub fn render_remote_instance_transcript_with_options_no_retry(
    db: &HcomDb,
    name: &str,
    display_name: &str,
    device: &str,
    opts: &TranscriptRenderOpts<'_>,
) -> Result<String, String> {
    render_instance_transcript_impl(db, name, opts, display_name, Some(device))
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
        name,
        None,
    )
}

fn render_instance_transcript_impl(
    db: &HcomDb,
    name: &str,
    opts: &TranscriptRenderOpts<'_>,
    display_name: &str,
    device: Option<&str>,
) -> Result<String, String> {
    let (instance_name, transcript_path, agent_type, session_id) =
        resolve_instance_transcript(db, name)
            .ok_or_else(|| no_transcript_error(db, name, display_name, device))?;
    let range = opts.range.map(parse_range).transpose()?;
    let effective_last = if range.is_some() {
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
    )?;

    let filtered: Vec<&Exchange> = match range {
        Some((start, end)) => exchanges
            .iter()
            .filter(|e| e.position >= start && end.is_none_or(|end| e.position <= end))
            .collect(),
        None => exchanges.iter().collect(),
    };

    if filtered.is_empty()
        && let Some(msg) = range_miss_message(&exchanges, range)
    {
        return Err(msg);
    }

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
                    obj["usage"] = json!(ex.usage);
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
        return serde_json::to_string(&json_output).map_err(|e| e.to_string());
    }

    if filtered.is_empty() {
        return Ok("No exchanges found".to_string());
    }

    let header = conversation_header(&filtered, &exchanges, &instance_name);
    let owned: Vec<Exchange> = filtered.into_iter().cloned().collect();
    let mut formatted = format_transcript_exchanges(&owned, opts.full_mode, opts.detailed);
    if let Some(hint) = transcript::flag_hint(opts.full_mode, opts.detailed) {
        formatted.push('\n');
        formatted.push_str(hint);
    }
    Ok(format!("{header}\n\n{formatted}"))
}

/// Transcript `(path, tool, session_id)` for an exact agent name: the live row,
/// else the agent's last stopped snapshot. A live row without a transcript is
/// authoritative (None) rather than falling back to an older session.
pub(crate) fn exact_instance_transcript(
    db: &HcomDb,
    name: &str,
) -> Option<(String, String, Option<String>)> {
    match db.get_instance_full(name) {
        Ok(Some(instance)) if !instance.transcript_path.is_empty() => {
            return Some((instance.transcript_path, instance.tool, instance.session_id));
        }
        Ok(Some(_)) | Err(_) => return None,
        Ok(None) => {}
    }

    let (path, tool, sid) = db
        .conn()
        .query_row(
            "SELECT json_extract(data, '$.snapshot.transcript_path'), json_extract(data, '$.snapshot.tool'), json_extract(data, '$.snapshot.session_id') FROM events WHERE type = 'life' AND instance = ? AND json_extract(data, '$.action') = 'stopped' ORDER BY id DESC LIMIT 1",
            rusqlite::params![name],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .ok()?;
    let tool = tool.unwrap_or_else(|| detect_agent_type(&path).to_string());
    Some((path, tool, sid))
}

/// Resolve instance name to (name, transcript_path, agent_type, session_id).
fn resolve_instance_transcript(
    db: &HcomDb,
    name: &str,
) -> Option<(String, String, String, Option<String>)> {
    // An exact live or stopped identity is authoritative even when it has no
    // transcript. Only infer a prefix when no exact identity exists; otherwise
    // a transcript-bearing longer name can disclose the wrong conversation.
    if let Some(exact_name) = crate::identity::resolve_display_name_or_stopped(db, name) {
        return exact_instance_transcript(db, &exact_name)
            .map(|(path, tool, sid)| (exact_name, path, tool, sid));
    }

    // Prefix match (literal matching; only an unambiguous single match returns immediately)
    let escaped = name
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let pattern = format!("{escaped}%");

    if let Ok(mut stmt) = db.conn().prepare(
        "SELECT name, transcript_path, tool, session_id FROM instances WHERE name LIKE ?1 ESCAPE '\\' AND transcript_path IS NOT NULL AND transcript_path != '' LIMIT 2",
    ) {
        let rows_res = stmt
            .query_map(rusqlite::params![pattern], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .and_then(|mapped| mapped.collect::<rusqlite::Result<Vec<_>>>());

        if let Ok(matches) = rows_res
            && matches.len() == 1
        {
            return Some(matches.into_iter().next().unwrap());
        }
    }

    None
}

/// Inclusive exchange range; an open end runs to the last exchange.
type ExchangeRange = (usize, Option<usize>);

/// Parse "N", "N-M" or "N-" (N to the end). Positions start at 1.
fn parse_range(s: &str) -> Result<ExchangeRange, String> {
    let invalid = || format!("invalid range '{s}' (expected N, N-M or N-)");
    let position = |v: &str| v.parse::<usize>().ok().filter(|&v| v >= 1);
    match s.split_once('-') {
        None => position(s).map(|n| (n, Some(n))).ok_or_else(invalid),
        Some((start, "")) => position(start).map(|n| (n, None)).ok_or_else(invalid),
        Some((start, end)) => match (position(start), position(end)) {
            (Some(start), Some(end)) if start <= end => Ok((start, Some(end))),
            _ => Err(invalid()),
        },
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::shared::finalize_action_text;
    use crate::transcript::{ToolUse, summarize_action};
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
    fn known_agent_without_transcript_points_to_transport_history() {
        let db = test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, created_at, transcript_path, tool) \
             VALUES ('pita', 100.0, '', 'adhoc')",
                [],
            )
            .unwrap();

        let error = no_transcript_error(&db, "pita", "pita", None);
        assert_eq!(
            error,
            "No model transcript is registered for pita.\n\
View transport messages with: hcom events --participant pita --type message"
        );
        assert!(!error.contains("no messages have been exchanged"));
    }

    #[test]
    fn remote_agent_without_transcript_queries_its_origin_device() {
        let db = test_db();
        db.conn()
            .execute(
                "INSERT INTO instances (name, created_at, transcript_path, tool) \
                 VALUES ('pita', 100.0, '', 'adhoc')",
                [],
            )
            .unwrap();

        let error = no_transcript_error(&db, "pita", "pita:ABCD", Some("ABCD"));
        assert_eq!(
            error,
            "No model transcript is registered for pita:ABCD.\n\
View transport messages with: hcom events --remote-fetch --device ABCD --participant pita --type message"
        );
    }

    #[test]
    fn test_parse_range() {
        assert_eq!(parse_range("5"), Ok((5, Some(5))));
        assert_eq!(parse_range("3-10"), Ok((3, Some(10))));
        assert_eq!(parse_range("3-"), Ok((3, None)));
        for bad in ["abc", "0", "5-3", "-3", "1-x", ""] {
            assert!(parse_range(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn test_centered_snippet_shows_match_deep_in_long_line() {
        // A whole-JSON transcript line: the match sits far past the start, where
        // start-anchored truncation would never reach it.
        let prefix = "{\"parentUuid\":null,".repeat(40); // long metadata head
        let line = format!("{prefix}\"text\":\"NEEDLE here\"}}");
        let col = line.find("NEEDLE").unwrap() + 1; // rg is 1-based
        let snip = centered_snippet(&line, col, SEARCH_SNIPPET_WIDTH);
        assert!(snip.contains("NEEDLE"), "match must be visible: {snip}");
        assert!(snip.starts_with('…'), "elided head marked: {snip}");
        assert!(snip.len() <= SEARCH_SNIPPET_WIDTH + 8); // window + ellipses/trim slack
    }

    #[test]
    fn test_centered_snippet_short_line_returned_whole() {
        let line = "{\"text\":\"hi ping there\"}";
        let col = line.find("ping").unwrap() + 1;
        let snip = centered_snippet(line, col, SEARCH_SNIPPET_WIDTH);
        assert_eq!(snip, line, "short line kept intact, no ellipsis");
    }

    #[test]
    fn test_centered_snippet_multibyte_safe() {
        // Match adjacent to multi-byte chars; slicing must not panic and must
        // stay on char boundaries.
        let line = format!("{}émoji→NEEDLE←café{}", "🚀".repeat(60), "ü".repeat(60));
        let col = line.find("NEEDLE").unwrap() + 1;
        let snip = centered_snippet(&line, col, SEARCH_SNIPPET_WIDTH);
        assert!(snip.contains("NEEDLE"));
        assert!(std::str::from_utf8(snip.as_bytes()).is_ok());
    }

    #[test]
    fn test_parse_match_line_rg_with_column() {
        // rg --column format: LINE:COL:TEXT — COL points at the match.
        let head = "x".repeat(300);
        let text = format!("{head}FINDME{head}");
        let col = 301; // 1-based, right after the 300-char head
        let raw = format!("44:{col}:{text}");
        let (line, snip) = parse_match_line(&raw);
        assert_eq!(line, 44);
        assert!(snip.contains("FINDME"), "centered on rg column: {snip}");
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
            detect_agent_type("/home/user/.gemini/tmp/project/chats/session-1-abc.json"),
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
        assert_eq!(
            detect_agent_type("/home/user/.local/share/kilo/kilo.db"),
            "kilo"
        );
        assert_eq!(
            detect_agent_type("/home/user/Library/Application Support/Antigravity/session.jsonl"),
            "antigravity"
        );
        assert_eq!(
            detect_agent_type("/home/user/.copilot/session-state/abc/events.jsonl"),
            "copilot"
        );
        assert_eq!(
            detect_agent_type("/home/user/.qoder/projects/-home-user-proj/abc.jsonl"),
            "qoder"
        );
    }

    #[test]
    fn detect_agent_type_covers_released_integrations_with_transcript_parsers() {
        let cases = [
            ("/home/user/.claude/projects/x/transcript.jsonl", "claude"),
            (
                "/home/user/.gemini/tmp/project/chats/session-1-abc.json",
                "gemini",
            ),
            ("/home/user/.codex/sessions/x/rollout.jsonl", "codex"),
            ("/home/user/.local/share/opencode/opencode.db", "opencode"),
            ("/home/user/.local/share/kilo/kilo.db", "kilo"),
            (
                "/home/user/Library/Application Support/Antigravity/session.jsonl",
                "antigravity",
            ),
            (
                "/home/user/.cursor/projects/x/agent-transcripts/abc/abc.jsonl",
                "cursor",
            ),
            (
                "/home/user/.kimi-code/sessions/wd_x/abc123/agents/main/wire.jsonl",
                "kimi",
            ),
            (
                "/home/user/.copilot/session-state/abc/events.jsonl",
                "copilot",
            ),
            (
                "/home/user/.qoder/projects/-home-user-proj/abc.jsonl",
                "qoder",
            ),
            (
                "/home/user/.local/share/devin/cli/transcripts/oasis-jump.json",
                "devin",
            ),
            (
                "/home/user/.grok/sessions/%2Fhome%2Fuser%2Fproj/019f-uuid/updates.jsonl",
                "grok",
            ),
            ("/home/user/.pi/agent/sessions/x/20260603_abc.jsonl", "pi"),
            ("/home/user/.omp/agent/sessions/x/20260603_abc.jsonl", "omp"),
            ("/home/user/.hermes/state.db", "hermes"),
        ];
        let expected: std::collections::HashSet<&str> =
            crate::integration_spec::released_tool_names()
                .into_iter()
                .collect();
        let actual: std::collections::HashSet<&str> = cases
            .iter()
            .map(|(path, expected_tool)| {
                let detected = detect_agent_type(path);
                assert_eq!(detected, *expected_tool);
                detected
            })
            .collect();

        assert_eq!(
            actual, expected,
            "transcript path detection cases must cover every released integration"
        );
    }

    #[test]
    fn attribute_disk_match_uses_provenance_for_unsignatured_pi_sessions() {
        let pi_root = PathBuf::from("/data/pi-sessions");
        let gem_root = PathBuf::from("/home/u/.gemini");
        let owners = vec![
            (pi_root.clone(), Tool::Pi),
            // gemini and antigravity share one root — the ambiguous case.
            (gem_root.clone(), Tool::Gemini),
            (gem_root.clone(), Tool::Antigravity),
        ];
        let selected = [Tool::Pi, Tool::Gemini, Tool::Antigravity];

        // A bare uuid.jsonl under a custom PI_CODING_AGENT_SESSION_DIR has no
        // content signature, so it is attributed by provenance.
        assert_eq!(
            attribute_disk_match("/data/pi-sessions/abc/9f8e.jsonl", &selected, &owners),
            Some(Tool::Pi)
        );
        // A signatured gemini file under the shared root resolves by content.
        assert_eq!(
            attribute_disk_match(
                "/home/u/.gemini/tmp/p/chats/session-1-x.json",
                &selected,
                &owners
            ),
            Some(Tool::Gemini)
        );
        // An unsignatured file under the shared gemini/antigravity root is
        // ambiguous by provenance and must not be guessed.
        assert_eq!(
            attribute_disk_match("/home/u/.gemini/tmp/p/notes.jsonl", &selected, &owners),
            None
        );
        // Provenance only counts roots for selected tools.
        assert_eq!(
            attribute_disk_match("/data/pi-sessions/abc/9f8e.jsonl", &[Tool::Gemini], &owners),
            None
        );
    }

    #[test]
    fn detect_agent_type_cursor_keys_on_agent_transcripts_not_dotcursor() {
        // Regression: a Claude transcript path with a LITERAL `.cursor` segment
        // (the CLAUDE_CONFIG_DIR-style vector the old `.contains(".cursor")`
        // matcher WOULD have misrouted to cursor) must detect claude. Feeds
        // resume tool detection → wrong match would break resume + parser.
        assert_eq!(
            detect_agent_type("/home/u/.claude/projects/x/.cursor/abcd.jsonl"),
            "claude"
        );
        // A real cursor transcript (the `agent-transcripts` segment) detects cursor.
        assert_eq!(
            detect_agent_type("/home/u/.cursor/projects/repo/agent-transcripts/uuid/uuid.jsonl"),
            "cursor"
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
            output: None,
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

        assert!(rendered.contains("Recent conversation (1 exchange, 2 of 2) - @luna:"));
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
    fn test_render_antigravity_transcript_user_input_and_planner_response() {
        let dir = tempfile::tempdir().unwrap();
        let transcript_path = dir.path().join("Antigravity-session.jsonl");
        let db = test_db();
        let now = crate::shared::time::now_epoch_f64();
        let lines = [
            json!({
                "type": "USER_INPUT",
                "timestamp": "2026-03-27T10:00:00Z",
                "text": "review the hook changes"
            }),
            json!({
                "type": "PLANNER_RESPONSE",
                "timestamp": "2026-03-27T10:00:01Z",
                "text": "I will inspect the Antigravity hook path."
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
        data.insert("tool".into(), json!("antigravity"));
        data.insert(
            "transcript_path".into(),
            json!(transcript_path.to_string_lossy().to_string()),
        );
        db.save_instance_named("vibo", &data).unwrap();

        let rendered =
            render_instance_transcript_with_options(&db, "vibo", None, 10, false, false, false)
                .unwrap();

        assert!(rendered.contains("review the hook changes"));
        assert!(rendered.contains("inspect the Antigravity hook path."));
        assert!(!rendered.contains("No exchanges found"));
    }

    #[test]
    fn test_finalize_action_text_uses_final_error_state_only() {
        let tools = vec![
            ToolUse {
                name: "Edit".to_string(),
                is_error: true,
                file: Some("a.rs".to_string()),
                command: None,
                output: None,
            },
            ToolUse {
                name: "Edit".to_string(),
                is_error: false,
                file: Some("a.rs".to_string()),
                command: None,
                output: None,
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

    #[test]
    fn missing_search_tool_is_an_error_not_an_empty_result() {
        let err =
            run_search_tool("__hcom_definitely_missing_search_tool__", &["pattern"]).unwrap_err();
        assert!(err.contains("was not found on PATH"));
    }

    fn insert_test_instance(db: &HcomDb, name: &str, transcript_path: &str, tool: &str) {
        let mut data = serde_json::Map::new();
        data.insert(
            "created_at".into(),
            json!(crate::shared::time::now_epoch_f64()),
        );
        data.insert("tool".into(), json!(tool));
        data.insert("transcript_path".into(), json!(transcript_path));
        db.save_instance_named(name, &data).unwrap();
    }

    #[test]
    fn test_resolve_instance_transcript_literal_underscore_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db();

        let p_testa = dir.path().join("testa1.jsonl");
        fs::write(&p_testa, "").unwrap();
        insert_test_instance(&db, "testa1", p_testa.to_str().unwrap(), "codex");

        // Literal '_' in requested prefix must not match 'testa1' via SQL LIKE wildcard
        let res = resolve_instance_transcript(&db, "test_");
        assert_eq!(
            res, None,
            "literal '_' must not match 'testa1' via SQL wildcard"
        );
    }

    #[test]
    fn test_resolve_instance_transcript_literal_percent_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db();

        let p_fooa = dir.path().join("fooa1.jsonl");
        fs::write(&p_fooa, "").unwrap();
        insert_test_instance(&db, "fooa1", p_fooa.to_str().unwrap(), "codex");

        // Literal '%' in requested prefix must not match 'fooa1' via SQL LIKE wildcard
        let res = resolve_instance_transcript(&db, "foo%");
        assert_eq!(
            res, None,
            "literal '%' must not match 'fooa1' via SQL wildcard"
        );
    }

    #[test]
    fn test_resolve_instance_transcript_literal_backslash_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db();

        let p_slash = dir.path().join("esc_slash.jsonl");
        fs::write(&p_slash, "").unwrap();
        insert_test_instance(&db, "esc\\1", p_slash.to_str().unwrap(), "codex");

        // Literal backslash in prefix must resolve literal instance 'esc\1'
        let res = resolve_instance_transcript(&db, "esc\\");
        assert_eq!(
            res.as_ref().map(|(n, _, _, _)| n.as_str()),
            Some("esc\\1"),
            "literal backslash prefix 'esc\\' must resolve to 'esc\\1'"
        );
    }

    #[test]
    fn test_resolve_instance_transcript_ambiguous_prefix_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db();

        let p1 = dir.path().join("ambig_one.jsonl");
        let p2 = dir.path().join("ambig_two.jsonl");
        fs::write(&p1, "").unwrap();
        fs::write(&p2, "").unwrap();

        insert_test_instance(&db, "ambig_one", p1.to_str().unwrap(), "codex");
        insert_test_instance(&db, "ambig_two", p2.to_str().unwrap(), "codex");

        // Multiple candidates matching prefix must fail closed (return None)
        // rather than picking an arbitrary candidate based on SQLite row order
        let resolved = resolve_instance_transcript(&db, "ambig_");
        assert_eq!(
            resolved, None,
            "multiple prefix candidates must fail closed to avoid disclosing the wrong transcript"
        );
    }

    #[test]
    fn test_resolve_instance_transcript_exact_match_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db();

        let p_exact = dir.path().join("exact.jsonl");
        let p_longer = dir.path().join("exact_one.jsonl");
        fs::write(&p_exact, "").unwrap();
        fs::write(&p_longer, "").unwrap();

        insert_test_instance(&db, "exact", p_exact.to_str().unwrap(), "codex");
        insert_test_instance(&db, "exact_one", p_longer.to_str().unwrap(), "codex");

        // Exact match must win immediately, without triggering prefix ambiguity
        let resolved = resolve_instance_transcript(&db, "exact");
        assert!(resolved.is_some(), "exact match must resolve");
        let (name, path, tool, _) = resolved.unwrap();
        assert_eq!(name, "exact");
        assert_eq!(path, p_exact.to_str().unwrap());
        assert_eq!(tool, "codex");
    }

    #[test]
    fn test_resolve_instance_transcript_exact_live_without_transcript_blocks_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db();

        db.conn()
            .execute(
                "INSERT INTO instances (name, created_at, transcript_path, tool) VALUES ('pico', 1.0, '', 'adhoc')",
                [],
            )
            .unwrap();
        let p_longer = dir.path().join("pico_worker.jsonl");
        fs::write(&p_longer, "").unwrap();
        insert_test_instance(&db, "pico_worker", p_longer.to_str().unwrap(), "codex");

        assert_eq!(
            resolve_instance_transcript(&db, "pico"),
            None,
            "an exact live identity without a transcript must not borrow a prefix match"
        );
    }

    #[test]
    fn test_resolve_instance_transcript_exact_stopped_preempts_unique_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db();

        let p_longer = dir.path().join("pico_worker.jsonl");
        let p_stopped = dir.path().join("pico_stopped.jsonl");
        fs::write(&p_longer, "").unwrap();
        fs::write(&p_stopped, "").unwrap();
        insert_test_instance(&db, "pico_worker", p_longer.to_str().unwrap(), "codex");
        db.conn()
            .execute(
                "INSERT INTO events (timestamp, type, instance, data) VALUES (?1, 'life', 'pico', ?2)",
                rusqlite::params![
                    "2026-03-27T10:00:00Z",
                    json!({
                        "action": "stopped",
                        "snapshot": {
                            "transcript_path": p_stopped.to_str().unwrap(),
                            "session_id": "sess-pico-stopped"
                        }
                    })
                    .to_string()
                ],
            )
            .unwrap();

        let resolved = resolve_instance_transcript(&db, "pico").unwrap();
        assert_eq!(resolved.0, "pico");
        assert_eq!(resolved.1, p_stopped.to_str().unwrap());
        assert_eq!(resolved.3.as_deref(), Some("sess-pico-stopped"));
    }

    #[test]
    fn test_resolve_instance_transcript_exact_live_without_transcript_blocks_stale_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db();

        db.conn()
            .execute(
                "INSERT INTO instances (name, created_at, transcript_path, tool) VALUES ('pico', 1.0, '', 'adhoc')",
                [],
            )
            .unwrap();
        let p_stopped = dir.path().join("pico_stopped.jsonl");
        fs::write(&p_stopped, "").unwrap();
        db.conn()
            .execute(
                "INSERT INTO events (timestamp, type, instance, data) VALUES (?1, 'life', 'pico', ?2)",
                rusqlite::params![
                    "2026-03-27T10:00:00Z",
                    json!({
                        "action": "stopped",
                        "snapshot": {"transcript_path": p_stopped.to_str().unwrap()}
                    })
                    .to_string()
                ],
            )
            .unwrap();

        assert_eq!(
            resolve_instance_transcript(&db, "pico"),
            None,
            "a current exact identity must not inherit an older incarnation's transcript"
        );
    }

    #[test]
    fn test_resolve_instance_transcript_stopped_instance_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db();

        let session_dir = dir.path().join(".codex/sessions/sess-stopped-123");
        fs::create_dir_all(&session_dir).unwrap();
        let p_stopped = session_dir.join("rollout.jsonl");
        fs::write(&p_stopped, "").unwrap();

        // Insert a stopped life event into the events table
        db.conn()
            .execute(
                "INSERT INTO events (timestamp, type, instance, data) VALUES (?1, 'life', ?2, ?3)",
                rusqlite::params![
                    "2026-03-27T10:00:00Z",
                    "miso",
                    json!({
                        "action": "stopped",
                        "snapshot": {
                            "transcript_path": p_stopped.to_str().unwrap(),
                            "session_id": "sess-stopped-123"
                        }
                    })
                    .to_string()
                ],
            )
            .unwrap();

        let res = resolve_instance_transcript(&db, "miso");
        assert!(
            res.is_some(),
            "stopped instance should resolve via events table fallback"
        );
        let (name, path, tool, sid) = res.unwrap();
        assert_eq!(name, "miso");
        assert_eq!(path, p_stopped.to_str().unwrap());
        assert_eq!(tool, "codex");
        assert_eq!(sid.as_deref(), Some("sess-stopped-123"));
    }

    #[test]
    fn test_resolve_instance_transcript_single_literal_prefix_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db();

        let p_sole = dir.path().join("sole_worker.jsonl");
        fs::write(&p_sole, "").unwrap();
        insert_test_instance(&db, "sole_worker", p_sole.to_str().unwrap(), "codex");

        // A single unambiguous literal prefix must resolve to its sole candidate
        let res = resolve_instance_transcript(&db, "sole_");
        assert!(
            res.is_some(),
            "unambiguous single prefix match should resolve"
        );
        let (name, path, tool, _) = res.unwrap();
        assert_eq!(name, "sole_worker");
        assert_eq!(path, p_sole.to_str().unwrap());
        assert_eq!(tool, "codex");
    }

    #[test]
    fn test_resolve_instance_transcript_stopped_instance_preempts_ambiguous_prefix_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let db = test_db();

        // Two active prefix candidates starting with "pico_"
        let p_active1 = dir.path().join("pico_one.jsonl");
        let p_active2 = dir.path().join("pico_two.jsonl");
        fs::write(&p_active1, "").unwrap();
        fs::write(&p_active2, "").unwrap();
        insert_test_instance(&db, "pico_one", p_active1.to_str().unwrap(), "codex");
        insert_test_instance(&db, "pico_two", p_active2.to_str().unwrap(), "codex");

        // One stopped instance whose exact name matches input "pico"
        let session_dir = dir.path().join(".codex/sessions/sess-pico-stopped");
        fs::create_dir_all(&session_dir).unwrap();
        let p_stopped = session_dir.join("rollout.jsonl");
        fs::write(&p_stopped, "").unwrap();

        db.conn()
            .execute(
                "INSERT INTO events (timestamp, type, instance, data) VALUES (?1, 'life', ?2, ?3)",
                rusqlite::params![
                    "2026-03-27T10:00:00Z",
                    "pico",
                    json!({
                        "action": "stopped",
                        "snapshot": {
                            "transcript_path": p_stopped.to_str().unwrap(),
                            "session_id": "sess-pico-stopped"
                        }
                    })
                    .to_string()
                ],
            )
            .unwrap();

        let res = resolve_instance_transcript(&db, "pico");
        assert!(
            res.is_some(),
            "exact stopped instance must resolve when active prefix candidates are ambiguous"
        );
        let (name, path, tool, sid) = res.unwrap();
        assert_eq!(
            name, "pico",
            "must resolve stopped instance, not an active prefix candidate"
        );
        assert_eq!(path, p_stopped.to_str().unwrap());
        assert_eq!(tool, "codex");
        assert_eq!(sid.as_deref(), Some("sess-pico-stopped"));
    }

    fn insert_stopped(db: &HcomDb, name: &str, ts: &str, path: &str, session_id: &str) {
        db.conn()
            .execute(
                "INSERT INTO events (timestamp, type, instance, data) VALUES (?1, 'life', ?2, ?3)",
                rusqlite::params![
                    ts,
                    name,
                    json!({"action": "stopped", "snapshot": {
                        "transcript_path": path, "tool": "claude", "session_id": session_id
                    }})
                    .to_string()
                ],
            )
            .unwrap();
    }

    #[test]
    fn tracked_transcripts_skip_empty_paths_and_order_stopped_newest_first() {
        let db = test_db();
        insert_test_instance(&db, "live", "/t/live.jsonl", "claude");
        insert_stopped(&db, "oldy", "2026-01-01T00:00:00Z", "/t/old.jsonl", "s-old");
        // A transcript-less snapshot must not hide the same agent's real one.
        insert_stopped(
            &db,
            "peso",
            "2026-01-02T00:00:00Z",
            "/t/peso.jsonl",
            "s-peso",
        );
        insert_stopped(&db, "peso", "2026-01-03T00:00:00Z", "", "");
        // One transcript per name: the newest, which is what `hcom transcript
        // newy` opens, so result references never point at another session.
        insert_stopped(
            &db,
            "newy",
            "2026-01-04T00:00:00Z",
            "/t/older.jsonl",
            "s-older",
        );
        insert_stopped(&db, "newy", "2026-01-05T00:00:00Z", "/t/new.jsonl", "s-new");

        let found: Vec<(String, String)> = tracked_transcripts(&db, false)
            .into_iter()
            .map(|t| (t.name, t.path))
            .collect();
        let expected = [
            ("live", "/t/live.jsonl"),
            ("newy", "/t/new.jsonl"),
            ("peso", "/t/peso.jsonl"),
            ("oldy", "/t/old.jsonl"),
        ]
        .map(|(n, p)| (n.to_string(), p.to_string()));
        assert_eq!(found, expected);

        let live: Vec<String> = tracked_transcripts(&db, true)
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(live, ["live"]);
    }

    #[test]
    fn match_exchange_checks_prompt_then_response_then_tools() {
        let mut ex = Exchange {
            usage: None,
            position: 3,
            user: "please fix the parser".to_string(),
            action: "Fixed the parser in claude.rs.".to_string(),
            final_text: String::new(),
            files: vec!["src/transcript/claude.rs".to_string()],
            timestamp: String::new(),
            tools: vec![ToolUse {
                name: "Bash".to_string(),
                is_error: false,
                file: None,
                command: Some("cargo test --bin hcom".to_string()),
                output: Some("test result: FAILED. 1 failed".to_string()),
            }],
            edits: Vec::new(),
            errors: vec![json!({"tool": "Bash", "content": "exit status 101"})],
            ended_on_error: false,
        };
        let re = |p: &str| regex::Regex::new(p).unwrap();
        assert_eq!(match_exchange(&ex, &re("parser")).unwrap().0, "user");
        assert_eq!(
            match_exchange(&ex, &re("claude\\.rs")).unwrap().0,
            "assistant"
        );
        assert_eq!(match_exchange(&ex, &re("cargo test")).unwrap().0, "tool");
        assert_eq!(match_exchange(&ex, &re("1 failed")).unwrap().0, "tool");
        assert_eq!(match_exchange(&ex, &re("status 101")).unwrap().0, "tool");
        assert!(match_exchange(&ex, &re("nowhere")).is_none());

        // Snippets are single-line even when the matched text spans lines.
        ex.user = "line one\nline two".to_string();
        assert_eq!(
            match_exchange(&ex, &re("two")).unwrap().1,
            "line one line two"
        );
    }

    #[test]
    fn prefilter_is_skipped_for_patterns_json_escaping_could_hide() {
        assert!(
            prefilter_files(r#"say "hi""#, &["/x.jsonl"])
                .unwrap()
                .is_none()
        );
        assert!(prefilter_files(r"a\nb", &["/x.jsonl"]).unwrap().is_none());
        assert!(prefilter_files("^Fix", &["/x.jsonl"]).unwrap().is_none());
        assert!(prefilter_files("done$", &["/x.jsonl"]).unwrap().is_none());
        assert!(
            prefilter_files("(?s)alpha.beta", &["/x.jsonl"])
                .unwrap()
                .is_none()
        );
        assert!(
            prefilter_files(r"foo\s+bar", &["/x.jsonl"])
                .unwrap()
                .is_none()
        );
        assert!(prefilter_files("naïve", &["/x.jsonl"]).unwrap().is_none());
        assert!(prefilter_files("anything", &[]).unwrap().is_none());
    }

    #[test]
    fn parse_timestamp_accepts_rfc3339_and_epochs() {
        let expected = chrono::DateTime::parse_from_rfc3339("2026-10-14T17:00:00Z").unwrap();
        assert_eq!(
            parse_timestamp("2026-10-14T17:00:00Z"),
            Some(expected.into())
        );
        assert_eq!(
            parse_timestamp("2026-10-14T18:00:00+01:00"),
            Some(expected.into())
        );
        assert_eq!(parse_timestamp("1791997200"), Some(expected.into()));
        assert_eq!(parse_timestamp("1791997200000"), Some(expected.into()));
        assert_eq!(parse_timestamp("not a time"), None);
        assert_eq!(format_local_time("garbage"), "??:??");
    }
}
