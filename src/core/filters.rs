//! Composable event filter system for queries, subscriptions, and listen.
//!
//! - `hcom events` (queries)
//! - `hcom events sub` (subscriptions)
//! - `hcom listen` (blocking waits)
//!
//! Flag parsing extracts known filter flags from argv, SQL generation builds
//! WHERE clauses from parsed filters.

use std::collections::HashMap;

use crate::shared::constants::{ST_BLOCKED, ST_LISTENING};

/// Mapping of CLI flags to internal filter keys.
const FLAG_MAP: &[(&str, &str)] = &[
    ("--agent", "instance"),
    ("--type", "type"),
    ("--status", "status"),
    ("--context", "context"),
    ("--file", "file"),
    ("--cmd", "cmd"),
    ("--from", "from"),
    ("--mention", "mention"),
    ("--action", "action"),
    ("--after", "after"),
    ("--before", "before"),
    ("--intent", "intent"),
    ("--thread", "thread"),
    ("--reply-to", "reply_to"),
    ("--collision", "collision"),
];

/// Flags that require type='status'.
const STATUS_FLAGS: &[&str] = &["status", "context", "file", "cmd"];
/// Flags that require type='message'.
const MESSAGE_FLAGS: &[&str] = &["from", "mention", "intent", "thread", "reply_to"];
/// Flags that require type='life'.
const LIFE_FLAGS: &[&str] = &["action"];

/// File-write tool contexts for SQL filters.
pub const FILE_WRITE_CONTEXTS: &str = "('tool:Write', 'tool:Edit', 'tool:write_file', 'tool:replace', 'tool:apply_patch', 'tool:write', 'tool:edit')";

/// All file operation contexts.
pub const FILE_OP_CONTEXTS: &[&str] = &[
    "tool:Write",
    "tool:Edit",
    "tool:Read",
    "tool:write_file",
    "tool:replace",
    "tool:read_file",
    "tool:apply_patch",
];

/// Shell tool contexts.
pub const SHELL_TOOL_CONTEXTS: &str = "('tool:Bash', 'tool:run_shell_command', 'tool:shell')";

/// Parsed filter values — multiple values per key (OR semantics).
pub type FilterMap = HashMap<String, Vec<String>>;

fn flag_to_key(flag: &str) -> Option<&'static str> {
    FLAG_MAP.iter().find(|(f, _)| *f == flag).map(|(_, k)| *k)
}

/// Expand shortcut flags to full flags.
///
/// - `--idle NAME` -> `--agent NAME --status listening`
/// - `--blocked NAME` -> `--agent NAME --status blocked`
pub fn expand_shortcuts(argv: &[String]) -> Vec<String> {
    let mut expanded = Vec::with_capacity(argv.len());
    let mut i = 0;

    while i < argv.len() {
        if argv[i] == "--idle" && i + 1 < argv.len() {
            expanded.push("--agent".into());
            expanded.push(argv[i + 1].clone());
            expanded.push("--status".into());
            expanded.push(ST_LISTENING.into());
            i += 2;
        } else if argv[i] == "--blocked" && i + 1 < argv.len() {
            expanded.push("--agent".into());
            expanded.push(argv[i + 1].clone());
            expanded.push("--status".into());
            expanded.push(ST_BLOCKED.into());
            i += 2;
        } else {
            expanded.push(argv[i].clone());
            i += 1;
        }
    }

    expanded
}

/// Parse event filter flags from argv.
///
/// Returns (filters, remaining_argv). Multiple instances of the same flag
/// are collected into a list (OR semantics).
///
/// After parsing, call `resolve_filter_names(&mut filters, db)` to resolve
/// tag-name display names (e.g., "team-luna" → "luna") for instance filters.
/// Call `resolve_filter_names` after parsing to resolve tag-name display names.
pub fn parse_event_flags(argv: &[String]) -> Result<(FilterMap, Vec<String>), String> {
    let mut filters = FilterMap::new();
    let mut remaining = Vec::new();
    let mut i = 0;

    while i < argv.len() {
        let arg = &argv[i];

        if arg == "--collision" {
            // Boolean flag (no value)
            filters
                .entry("collision".into())
                .or_default()
                .push("true".into());
            i += 1;
        } else if let Some(key) = flag_to_key(arg) {
            if key == "collision" {
                // Already handled above
                filters
                    .entry("collision".into())
                    .or_default()
                    .push("true".into());
                i += 1;
            } else {
                if i + 1 >= argv.len() {
                    return Err(format!("Flag {} requires a value", arg));
                }
                let value = argv[i + 1].clone();
                filters.entry(key.into()).or_default().push(value);
                i += 2;
            }
        } else {
            remaining.push(arg.clone());
            i += 1;
        }
    }

    Ok((filters, remaining))
}

/// Resolve instance filter names through display-name/tag lookup.
///
/// Converts tag-name format ("team-luna") to base name ("luna") using the DB.
/// Must be called after `parse_event_flags` when a DB handle is available.
/// Without this, `--agent team-luna` won't match instance "luna" with tag "team".
///
///
pub fn resolve_filter_names(filters: &mut FilterMap, db: &crate::db::HcomDb) {
    if let Some(names) = filters.get_mut("instance") {
        let resolved: Vec<String> = names
            .iter()
            .map(|name| {
                crate::instances::resolve_display_name(db, name).unwrap_or_else(|| name.clone())
            })
            .collect();
        *names = resolved;
    }
}

/// Validate that filters don't mix incompatible event types.
pub fn validate_type_constraints(filters: &FilterMap) -> Result<(), String> {
    let mut required_types = Vec::new();

    if STATUS_FLAGS.iter().any(|f| filters.contains_key(*f)) {
        required_types.push("status");
    }
    if MESSAGE_FLAGS.iter().any(|f| filters.contains_key(*f)) {
        required_types.push("message");
    }
    if LIFE_FLAGS.iter().any(|f| filters.contains_key(*f)) {
        required_types.push("life");
    }

    // Explicit --type takes precedence
    if let Some(explicit) = filters.get("type") {
        let explicit_set: std::collections::HashSet<&str> =
            explicit.iter().map(|s| s.as_str()).collect();
        let conflicting: Vec<&&str> = required_types
            .iter()
            .filter(|t| !explicit_set.contains(**t))
            .collect();
        if !conflicting.is_empty() {
            return Err(format!(
                "Filters require type {{{}}} but --type specified {{{}}}",
                conflicting
                    .iter()
                    .map(|t| **t)
                    .collect::<Vec<_>>()
                    .join(", "),
                explicit.join(", ")
            ));
        }
    }

    if required_types.len() > 1 {
        required_types.sort();
        return Err(format!(
            "Cannot combine filters from different event types: {}\n\
             Status filters: --status, --context, --file, --cmd\n\
             Message filters: --from, --mention, --intent, --thread, --reply-to\n\
             Life filters: --action",
            required_types.join(", ")
        ));
    }

    Ok(())
}

/// Escape single quotes for SQL string literals.
fn escape_sql(s: &str) -> String {
    s.replace('\'', "''")
}

/// Escape LIKE wildcards and quotes for SQL LIKE patterns.
fn escape_sql_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
        .replace('\'', "''")
}

/// Generate `col = val` for single value, `col IN (...)` for multiple.
fn eq_or_in(column: &str, values: &[String]) -> String {
    if values.len() == 1 {
        format!("{} = '{}'", column, escape_sql(&values[0]))
    } else {
        let escaped: Vec<String> = values.iter().map(|x| escape_sql(x)).collect();
        format!("{} IN ('{}')", column, escaped.join("', '"))
    }
}

/// Wrap multiple clauses in OR, or return single clause unwrapped.
fn or_wrap(clauses: Vec<String>) -> String {
    if clauses.len() == 1 {
        clauses.into_iter().next().unwrap()
    } else {
        format!("({})", clauses.join(" OR "))
    }
}

/// Build SQL WHERE clause from filter flags.
///
/// Composition rules:
/// - Multiple values for same flag = OR (any can match)
/// - Different flags = AND (all must match)
/// - Automatically infers type based on filter flags used
pub fn build_sql_from_flags(filters: &FilterMap) -> Result<String, String> {
    if filters.is_empty() {
        return Ok(String::new());
    }

    validate_type_constraints(filters)?;

    let mut clauses: Vec<String> = Vec::new();

    // Instance filter
    if let Some(values) = filters.get("instance") {
        clauses.push(eq_or_in("instance", values));
    }

    // Type filter
    if let Some(values) = filters.get("type") {
        clauses.push(eq_or_in("type", values));
    } else if STATUS_FLAGS.iter().any(|f| filters.contains_key(*f)) {
        clauses.push("type = 'status'".into());
    } else if MESSAGE_FLAGS.iter().any(|f| filters.contains_key(*f)) {
        clauses.push("type = 'message'".into());
    } else if LIFE_FLAGS.iter().any(|f| filters.contains_key(*f)) {
        clauses.push("type = 'life'".into());
    }

    // Status filter
    if let Some(values) = filters.get("status") {
        clauses.push(eq_or_in("status_val", values));
    }

    // Context filter
    if let Some(values) = filters.get("context") {
        let context_clauses: Vec<String> = values
            .iter()
            .map(|pattern| {
                if pattern.contains('*') {
                    let parts: Vec<&str> = pattern.split('*').collect();
                    let sql_pattern: String = parts
                        .iter()
                        .map(|p| escape_sql_like(p))
                        .collect::<Vec<_>>()
                        .join("%");
                    format!("status_context LIKE '{}' ESCAPE '\\'", sql_pattern)
                } else {
                    format!("status_context = '{}'", escape_sql(pattern))
                }
            })
            .collect();
        clauses.push(or_wrap(context_clauses));
    }

    // File filter (status_detail for file write tools)
    if let Some(values) = filters.get("file") {
        clauses.push(format!("status_context IN {}", FILE_WRITE_CONTEXTS));
        let file_clauses: Vec<String> = values
            .iter()
            .map(|pattern| {
                if pattern.contains('*') {
                    let parts: Vec<&str> = pattern.split('*').collect();
                    let sql_pattern: String = parts
                        .iter()
                        .map(|p| escape_sql_like(p))
                        .collect::<Vec<_>>()
                        .join("%");
                    format!("status_detail LIKE '{}' ESCAPE '\\'", sql_pattern)
                } else {
                    format!(
                        "status_detail LIKE '%{}%' ESCAPE '\\'",
                        escape_sql_like(pattern)
                    )
                }
            })
            .collect();
        clauses.push(or_wrap(file_clauses));
    }

    // Cmd filter (status_detail for shell tools)
    // Supports =exact, ^starts-with, else contains — no $ or * glob
    if let Some(values) = filters.get("cmd") {
        clauses.push(format!("status_context IN {}", SHELL_TOOL_CONTEXTS));
        let cmd_clauses: Vec<String> = values
            .iter()
            .map(|pattern| {
                if let Some(stripped) = pattern.strip_prefix('=') {
                    format!("status_detail = '{}'", escape_sql(stripped))
                } else if let Some(stripped) = pattern.strip_prefix('^') {
                    format!(
                        "status_detail LIKE '{}%' ESCAPE '\\'",
                        escape_sql_like(stripped)
                    )
                } else {
                    format!(
                        "status_detail LIKE '%{}%' ESCAPE '\\'",
                        escape_sql_like(pattern)
                    )
                }
            })
            .collect();
        clauses.push(or_wrap(cmd_clauses));
    }

    // Message filters
    if let Some(values) = filters.get("from") {
        clauses.push(eq_or_in("msg_from", values));
    }

    if let Some(values) = filters.get("mention") {
        let mention_clauses: Vec<String> = values
            .iter()
            .map(|name| {
                format!(
                    "msg_mentions LIKE '%{}%' ESCAPE '\\'",
                    escape_sql_like(name)
                )
            })
            .collect();
        clauses.push(or_wrap(mention_clauses));
    }

    if let Some(values) = filters.get("intent") {
        clauses.push(eq_or_in("msg_intent", values));
    }

    if let Some(values) = filters.get("thread") {
        clauses.push(eq_or_in("msg_thread", values));
    }

    if let Some(values) = filters.get("reply_to") {
        clauses.push(eq_or_in("msg_reply_to", values));
    }

    // Life filters
    if let Some(values) = filters.get("action") {
        clauses.push(eq_or_in("life_action", values));
    }

    // Time range filters
    if let Some(values) = filters.get("after") {
        for ts in values {
            clauses.push(format!("timestamp >= '{}'", escape_sql(ts)));
        }
    }

    if let Some(values) = filters.get("before") {
        for ts in values {
            clauses.push(format!("timestamp < '{}'", escape_sql(ts)));
        }
    }

    // Collision filter
    if filters.contains_key("collision") {
        let collision_sql = format!(
            "(type = 'status' AND status_context IN {ctx}\n\
             AND EXISTS (\n\
             \x20   SELECT 1 FROM events_v e\n\
             \x20   WHERE e.type = 'status' AND e.status_context IN {ctx}\n\
             \x20   AND e.status_detail = events_v.status_detail\n\
             \x20   AND e.instance != events_v.instance\n\
             \x20   AND ABS(strftime('%s', events_v.timestamp) - strftime('%s', e.timestamp)) < 30\n\
             ))",
            ctx = FILE_WRITE_CONTEXTS
        );
        clauses.push(collision_sql);
    }

    Ok(clauses.join(" AND "))
}

/// Validate timestamp format: must start with YYYY-MM-DD.
/// YYYY-MM-DD: positions [0-3]=year, [4]='-', [5-6]=month, [7]='-', [8-9]=day
fn parse_timestamp(s: &str) -> Result<String, String> {
    let b = s.as_bytes();
    if b.len() >= 10
        && b[4] == b'-'
        && b[7] == b'-'
        && s[..4].parse::<u16>().is_ok()
        && s[5..7].parse::<u8>().is_ok()
        && s[8..10].parse::<u8>().is_ok()
    {
        Ok(s.to_string())
    } else {
        Err(format!(
            "'{s}' is not a valid timestamp (expected YYYY-MM-DD[THH:MM:SS[Z]])"
        ))
    }
}

/// Clap-compatible filter args for events, listen, events sub.
///
/// Replaces `parse_event_flags` + `expand_shortcuts` when used with clap.
/// Repeated flags use OR semantics (e.g., `--agent foo --agent bar`).
#[derive(clap::Args, Debug, Default, Clone)]
pub struct EventFilterArgs {
    #[arg(long)]
    pub agent: Vec<String>,
    #[arg(long = "type", value_parser = clap::builder::PossibleValuesParser::new(["message", "status", "life"]))]
    pub event_type: Vec<String>,
    #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(["active", "listening", "blocked", "inactive", "launching", "error"]))]
    pub status: Vec<String>,
    #[arg(long)]
    pub context: Vec<String>,
    #[arg(long)]
    pub file: Vec<String>,
    #[arg(long)]
    pub cmd: Vec<String>,
    #[arg(long)]
    pub from: Vec<String>,
    #[arg(long)]
    pub mention: Vec<String>,
    #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(["created", "started", "ready", "stopped", "batch_launched"]))]
    pub action: Vec<String>,
    #[arg(long, value_parser = parse_timestamp)]
    pub after: Vec<String>,
    #[arg(long, value_parser = parse_timestamp)]
    pub before: Vec<String>,
    #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(["request", "inform", "ack"]))]
    pub intent: Vec<String>,
    #[arg(long)]
    pub thread: Vec<String>,
    #[arg(long = "reply-to")]
    pub reply_to: Vec<String>,
    #[arg(long)]
    pub collision: bool,
    /// Shortcut: --idle NAME → --agent NAME --status listening
    #[arg(long)]
    pub idle: Vec<String>,
    /// Shortcut: --blocked NAME → --agent NAME --status blocked
    #[arg(long)]
    pub blocked: Vec<String>,
}

impl EventFilterArgs {
    /// Convert to FilterMap (expanding shortcuts), ready for `build_sql_from_flags`.
    pub fn to_filter_map(&self) -> FilterMap {
        let mut map = FilterMap::new();

        // Expand --idle/--blocked shortcuts into agent + status
        let mut agents = self.agent.clone();
        let mut statuses = self.status.clone();
        for name in &self.idle {
            agents.push(name.clone());
            statuses.push(crate::shared::constants::ST_LISTENING.to_string());
        }
        for name in &self.blocked {
            agents.push(name.clone());
            statuses.push(crate::shared::constants::ST_BLOCKED.to_string());
        }

        macro_rules! insert_if_nonempty {
            ($key:expr, $vec:expr) => {
                if !$vec.is_empty() {
                    map.insert($key.into(), $vec);
                }
            };
        }

        insert_if_nonempty!("instance", agents);
        insert_if_nonempty!("type", self.event_type.clone());
        insert_if_nonempty!("status", statuses);
        insert_if_nonempty!("context", self.context.clone());
        insert_if_nonempty!("file", self.file.clone());
        insert_if_nonempty!("cmd", self.cmd.clone());
        insert_if_nonempty!("from", self.from.clone());
        insert_if_nonempty!("mention", self.mention.clone());
        insert_if_nonempty!("action", self.action.clone());
        insert_if_nonempty!("after", self.after.clone());
        insert_if_nonempty!("before", self.before.clone());
        insert_if_nonempty!("intent", self.intent.clone());
        insert_if_nonempty!("thread", self.thread.clone());
        insert_if_nonempty!("reply_to", self.reply_to.clone());

        if self.collision {
            map.insert("collision".into(), vec!["true".into()]);
        }

        map
    }

    /// Returns true if any filter is set.
    pub fn has_filters(&self) -> bool {
        !self.agent.is_empty()
            || !self.event_type.is_empty()
            || !self.status.is_empty()
            || !self.context.is_empty()
            || !self.file.is_empty()
            || !self.cmd.is_empty()
            || !self.from.is_empty()
            || !self.mention.is_empty()
            || !self.action.is_empty()
            || !self.after.is_empty()
            || !self.before.is_empty()
            || !self.intent.is_empty()
            || !self.thread.is_empty()
            || !self.reply_to.is_empty()
            || self.collision
            || !self.idle.is_empty()
            || !self.blocked.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    // ===== expand_shortcuts =====

    #[test]
    fn test_expand_idle() {
        let result = expand_shortcuts(&s(&["--idle", "peso"]));
        assert_eq!(result, s(&["--agent", "peso", "--status", "listening"]));
    }

    #[test]
    fn test_expand_blocked() {
        let result = expand_shortcuts(&s(&["--blocked", "peso"]));
        assert_eq!(result, s(&["--agent", "peso", "--status", "blocked"]));
    }

    #[test]
    fn test_expand_passthrough() {
        let result = expand_shortcuts(&s(&["--last", "20", "--collision"]));
        assert_eq!(result, s(&["--last", "20", "--collision"]));
    }

    // ===== parse_event_flags =====

    #[test]
    fn test_parse_agent_flag() {
        let (filters, remaining) =
            parse_event_flags(&s(&["--agent", "peso", "--last", "20"])).unwrap();
        assert_eq!(filters["instance"], vec!["peso"]);
        assert_eq!(remaining, s(&["--last", "20"]));
    }

    #[test]
    fn test_parse_collision_boolean() {
        let (filters, _) = parse_event_flags(&s(&["--collision"])).unwrap();
        assert!(filters.contains_key("collision"));
    }

    #[test]
    fn test_parse_missing_value() {
        let result = parse_event_flags(&s(&["--agent"]));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("requires a value"));
    }

    #[test]
    fn test_parse_multiple_same_flag() {
        let (filters, _) = parse_event_flags(&s(&["--agent", "peso", "--agent", "luna"])).unwrap();
        assert_eq!(filters["instance"], vec!["peso", "luna"]);
    }

    // ===== validate_type_constraints =====

    #[test]
    fn test_validate_no_conflict() {
        let mut filters = FilterMap::new();
        filters.insert("status".into(), vec!["listening".into()]);
        filters.insert("context".into(), vec!["tool:Write".into()]);
        assert!(validate_type_constraints(&filters).is_ok());
    }

    #[test]
    fn test_validate_conflict() {
        let mut filters = FilterMap::new();
        filters.insert("status".into(), vec!["listening".into()]);
        filters.insert("from".into(), vec!["bigboss".into()]);
        let err = validate_type_constraints(&filters).unwrap_err();
        assert!(err.contains("Cannot combine"));
    }

    // ===== build_sql_from_flags =====

    #[test]
    fn test_build_empty() {
        assert_eq!(build_sql_from_flags(&FilterMap::new()).unwrap(), "");
    }

    #[test]
    fn test_build_instance_status() {
        let mut filters = FilterMap::new();
        filters.insert("instance".into(), vec!["peso".into()]);
        filters.insert("status".into(), vec!["listening".into()]);
        let sql = build_sql_from_flags(&filters).unwrap();
        assert!(sql.contains("instance = 'peso'"));
        assert!(sql.contains("type = 'status'"));
        assert!(sql.contains("status_val = 'listening'"));
    }

    #[test]
    fn test_build_multi_instance() {
        let mut filters = FilterMap::new();
        filters.insert("instance".into(), vec!["peso".into(), "luna".into()]);
        let sql = build_sql_from_flags(&filters).unwrap();
        assert!(sql.contains("instance IN ('peso', 'luna')"));
    }

    #[test]
    fn test_build_cmd_exact() {
        let mut filters = FilterMap::new();
        filters.insert("cmd".into(), vec!["=git status".into()]);
        let sql = build_sql_from_flags(&filters).unwrap();
        assert!(sql.contains("status_detail = 'git status'"));
        assert!(sql.contains(SHELL_TOOL_CONTEXTS));
    }

    #[test]
    fn test_build_cmd_starts_with() {
        let mut filters = FilterMap::new();
        filters.insert("cmd".into(), vec!["^git".into()]);
        let sql = build_sql_from_flags(&filters).unwrap();
        assert!(sql.contains("status_detail LIKE 'git%'"));
    }

    #[test]
    fn test_build_cmd_contains_dollar_literal() {
        // $ is treated as literal in contains — no ends-with semantics
        let mut filters = FilterMap::new();
        filters.insert("cmd".into(), vec!["pattern$".into()]);
        let sql = build_sql_from_flags(&filters).unwrap();
        assert!(sql.contains("status_detail LIKE '%pattern$%'"));
    }

    #[test]
    fn test_build_cmd_contains_default() {
        let mut filters = FilterMap::new();
        filters.insert("cmd".into(), vec!["npm install".into()]);
        let sql = build_sql_from_flags(&filters).unwrap();
        assert!(sql.contains("status_detail LIKE '%npm install%'"));
    }

    #[test]
    fn test_build_file_glob() {
        let mut filters = FilterMap::new();
        filters.insert("file".into(), vec!["*.py".into()]);
        let sql = build_sql_from_flags(&filters).unwrap();
        assert!(sql.contains("status_detail LIKE '%.py'"));
        assert!(sql.contains(FILE_WRITE_CONTEXTS));
    }

    #[test]
    fn test_build_context_glob() {
        let mut filters = FilterMap::new();
        filters.insert("context".into(), vec!["tool:*".into()]);
        let sql = build_sql_from_flags(&filters).unwrap();
        assert!(sql.contains("status_context LIKE 'tool:%'"));
    }

    #[test]
    fn test_build_time_range() {
        let mut filters = FilterMap::new();
        filters.insert("after".into(), vec!["2024-01-01T00:00:00Z".into()]);
        filters.insert("before".into(), vec!["2024-12-31T23:59:59Z".into()]);
        let sql = build_sql_from_flags(&filters).unwrap();
        assert!(sql.contains("timestamp >= '2024-01-01T00:00:00Z'"));
        assert!(sql.contains("timestamp < '2024-12-31T23:59:59Z'"));
    }

    #[test]
    fn test_build_collision() {
        let mut filters = FilterMap::new();
        filters.insert("collision".into(), vec!["true".into()]);
        let sql = build_sql_from_flags(&filters).unwrap();
        assert!(sql.contains("EXISTS"));
        assert!(sql.contains("ABS(strftime"));
    }

    #[test]
    fn test_build_message_filters() {
        let mut filters = FilterMap::new();
        filters.insert("from".into(), vec!["bigboss".into()]);
        filters.insert("intent".into(), vec!["request".into()]);
        let sql = build_sql_from_flags(&filters).unwrap();
        assert!(sql.contains("msg_from = 'bigboss'"));
        assert!(sql.contains("msg_intent = 'request'"));
        assert!(sql.contains("type = 'message'"));
    }

    #[test]
    fn test_build_mention_filter() {
        let mut filters = FilterMap::new();
        filters.insert("mention".into(), vec!["luna".into(), "nova".into()]);
        let sql = build_sql_from_flags(&filters).unwrap();
        assert!(sql.contains("msg_mentions LIKE '%luna%'"));
        assert!(sql.contains("msg_mentions LIKE '%nova%'"));
        assert!(sql.contains(" OR "));
    }

    #[test]
    fn test_sql_injection_prevention() {
        let mut filters = FilterMap::new();
        filters.insert("instance".into(), vec!["O'Reilly".into()]);
        let sql = build_sql_from_flags(&filters).unwrap();
        assert!(sql.contains("O''Reilly"));
    }

    // ===== resolve_filter_names =====

    #[test]
    fn test_resolve_filter_names_with_tag() {
        // Create in-memory DB with an instance that has a tag
        let db = crate::db::HcomDb::open_raw(std::path::Path::new(":memory:")).unwrap();
        db.init_db().unwrap();

        // Insert instance "luna" with tag "team"
        db.conn()
            .execute(
                "INSERT INTO instances (name, status, tag, created_at) \
                 VALUES ('luna', 'active', 'team', strftime('%s','now'))",
                [],
            )
            .unwrap();

        // Parse --agent team-luna
        let (mut filters, _) = parse_event_flags(&s(&["--agent", "team-luna"])).unwrap();
        assert_eq!(filters["instance"], vec!["team-luna"]);

        // Resolve: "team-luna" should become "luna"
        resolve_filter_names(&mut filters, &db);
        assert_eq!(filters["instance"], vec!["luna"]);
    }

    #[test]
    fn test_resolve_filter_names_direct_match() {
        let db = crate::db::HcomDb::open_raw(std::path::Path::new(":memory:")).unwrap();
        db.init_db().unwrap();

        db.conn()
            .execute(
                "INSERT INTO instances (name, status, created_at) \
                 VALUES ('peso', 'active', strftime('%s','now'))",
                [],
            )
            .unwrap();

        let (mut filters, _) = parse_event_flags(&s(&["--agent", "peso"])).unwrap();
        resolve_filter_names(&mut filters, &db);
        // Should stay "peso" (direct match)
        assert_eq!(filters["instance"], vec!["peso"]);
    }

    #[test]
    fn test_resolve_filter_names_unknown_keeps_original() {
        let db = crate::db::HcomDb::open_raw(std::path::Path::new(":memory:")).unwrap();
        db.init_db().unwrap();

        let (mut filters, _) = parse_event_flags(&s(&["--agent", "nonexistent"])).unwrap();
        resolve_filter_names(&mut filters, &db);
        // Unknown name stays as-is
        assert_eq!(filters["instance"], vec!["nonexistent"]);
    }

    #[test]
    fn test_resolve_filter_names_no_instance_key() {
        let db = crate::db::HcomDb::open_raw(std::path::Path::new(":memory:")).unwrap();
        db.init_db().unwrap();

        let (mut filters, _) = parse_event_flags(&s(&["--status", "listening"])).unwrap();
        // Should not panic when no "instance" key
        resolve_filter_names(&mut filters, &db);
        assert!(!filters.contains_key("instance"));
    }

    // ===== EventFilterArgs =====

    #[test]
    fn test_filter_args_to_map_basic() {
        let args = EventFilterArgs {
            agent: vec!["peso".into()],
            event_type: vec!["message".into()],
            from: vec!["bigboss".into()],
            ..Default::default()
        };
        let map = args.to_filter_map();
        assert_eq!(map["instance"], vec!["peso"]);
        assert_eq!(map["type"], vec!["message"]);
        assert_eq!(map["from"], vec!["bigboss"]);
    }

    #[test]
    fn test_filter_args_idle_shortcut() {
        let args = EventFilterArgs {
            idle: vec!["peso".into()],
            ..Default::default()
        };
        let map = args.to_filter_map();
        assert_eq!(map["instance"], vec!["peso"]);
        assert_eq!(map["status"], vec!["listening"]);
    }

    #[test]
    fn test_filter_args_blocked_shortcut() {
        let args = EventFilterArgs {
            blocked: vec!["luna".into()],
            ..Default::default()
        };
        let map = args.to_filter_map();
        assert_eq!(map["instance"], vec!["luna"]);
        assert_eq!(map["status"], vec!["blocked"]);
    }

    #[test]
    fn test_filter_args_collision() {
        let args = EventFilterArgs {
            collision: true,
            ..Default::default()
        };
        let map = args.to_filter_map();
        assert!(map.contains_key("collision"));
    }

    #[test]
    fn test_filter_args_empty() {
        let args = EventFilterArgs::default();
        assert!(!args.has_filters());
        assert!(args.to_filter_map().is_empty());
    }

    #[test]
    fn test_filter_args_has_filters() {
        let args = EventFilterArgs {
            agent: vec!["peso".into()],
            ..Default::default()
        };
        assert!(args.has_filters());
    }

    #[test]
    fn test_filter_args_repeated_agents() {
        let args = EventFilterArgs {
            agent: vec!["peso".into(), "luna".into()],
            ..Default::default()
        };
        let map = args.to_filter_map();
        assert_eq!(map["instance"], vec!["peso", "luna"]);
    }
}
