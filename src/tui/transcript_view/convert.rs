//! Exchange (local) and transcript JSON rows (remote) into `TurnView`.
use std::collections::BTreeMap;

use serde_json::Value;

use super::model::{ToolSummary, TurnView, format_turn_time, parse_turn_epoch};
use crate::transcript::Exchange;
use crate::tui::model::Message;

pub const REQUEST_LINES: usize = 3;
pub const RESPONSE_TAIL_LINES: usize = 8;
pub const MAX_FILES: usize = 3;

/// Non-blank lines, indentation kept (code and nested lists).
fn nonempty_lines(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim_end().to_string())
        .collect()
}

/// Lines with trailing space removed, runs of blank lines collapsed to
/// one, and no blank lines at either end.
fn full_lines(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines().map(str::trim_end) {
        if line.is_empty() && out.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        out.push(line.to_string());
    }
    if out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

fn tag_body<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = text.find(&open)? + open.len();
    let end = start + text[start..].find(&format!("</{tag}>"))?;
    Some(text[start..end].trim())
}

fn strip_block(text: &str, tag: &str) -> String {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut out = text.to_string();
    while let Some(start) = out.find(&open) {
        let Some(end) = out[start..].find(&close) else {
            break;
        };
        out.replace_range(start..start + end + close.len(), "");
    }
    out
}

/// Claude wraps some user-turn text in tags: slash commands, `!` shell
/// commands and their output, local command output, and background task
/// results. Show each the way a person would read it. Only text that opens
/// with such a tag is Claude's: a request quoting one stays as written.
fn clean_request(user: &str) -> String {
    let opens = |text: &str, tags: &[&str]| {
        let text = text.trim_start();
        tags.iter().any(|tag| text.starts_with(&format!("<{tag}>")))
    };
    if !opens(
        user,
        &[
            "local-command-caveat",
            "command-message",
            "command-name",
            "task-notification",
            "bash-input",
            "bash-stdout",
            "bash-stderr",
            "local-command-stdout",
        ],
    ) {
        return user.trim().to_string();
    }
    let user = strip_block(user, "local-command-caveat");
    if opens(&user, &["command-message", "command-name"])
        && let Some(name) = tag_body(&user, "command-name")
    {
        let args = tag_body(&user, "command-args").unwrap_or("");
        return if args.is_empty() {
            name.to_string()
        } else {
            format!("{name} {args}")
        };
    }
    if opens(&user, &["task-notification"]) {
        let head = tag_body(&user, "summary")
            .or_else(|| tag_body(&user, "status"))
            .unwrap_or("task");
        let result = tag_body(&user, "result").unwrap_or("");
        return format!("[task] {head}\n{result}");
    }
    if opens(&user, &["bash-input"])
        && let Some(command) = tag_body(&user, "bash-input")
    {
        return format!("! {command}");
    }
    let output_tags = ["bash-stdout", "bash-stderr", "local-command-stdout"];
    if !opens(&user, &output_tags) {
        return user.trim().to_string();
    }
    let output: Vec<&str> = output_tags
        .iter()
        .filter_map(|tag| tag_body(&user, tag))
        .filter(|body| !body.is_empty())
        .collect();
    if !output.is_empty() {
        return output.join("\n");
    }
    user.trim().to_string()
}

fn build_turn(
    position: usize,
    timestamp: &str,
    user: &str,
    action: &str,
    tools: ToolSummary,
) -> TurnView {
    let hcom_wake = user.trim() == crate::delivery::WAKE_TRIGGER;
    let user = clean_request(user);
    let request = nonempty_lines(&user);
    let response = nonempty_lines(action);
    let omitted = response.len().saturating_sub(RESPONSE_TAIL_LINES);
    TurnView {
        position,
        time: format_turn_time(timestamp),
        request_more: request.len() > REQUEST_LINES,
        request_summary: request
            .first()
            .map(|l| l.trim().to_string())
            .unwrap_or_default(),
        request_lines: request.into_iter().take(REQUEST_LINES).collect(),
        response_summary: response
            .last()
            .map(|l| l.trim().to_string())
            .unwrap_or_default(),
        response_tail: response[omitted..].to_vec(),
        response_omitted: omitted,
        tools,
        request_full: full_lines(&user),
        response_full: full_lines(action),
        started_at: parse_turn_epoch(timestamp),
        hcom_wake,
    }
}

/// How far before the oldest shown turn a wake message may have been sent.
const WAKE_LOOKBACK_SECS: f64 = 1800.0;
/// Clock slack between the hcom event and the transcript timestamp.
const WAKE_SLACK_SECS: f64 = 2.0;

/// Replace each bare `<hcom>` request with the messages `agent` received
/// since the previous turn started: mentions of `agent`, or broadcasts.
pub fn fill_wake_requests(turns: &mut [TurnView], agent: &str, messages: &[Message]) {
    let mut previous: Option<f64> = None;
    for turn in turns.iter_mut() {
        let start = turn.started_at;
        if turn.hcom_wake
            && let Some(start) = start
        {
            let from = previous.unwrap_or(start - WAKE_LOOKBACK_SECS);
            let text: Vec<String> = messages
                .iter()
                .filter(|m| m.time > from && m.time <= start + WAKE_SLACK_SECS)
                .filter(|m| m.sender != agent)
                .filter(|m| {
                    m.recipients.iter().any(|r| r == agent)
                        || (m.recipients.is_empty() && !m.is_system())
                })
                .map(|m| format!("{}: {}", m.sender, m.body))
                .collect();
            if !text.is_empty() {
                let request = nonempty_lines(&text.join("\n"));
                turn.request_more = request.len() > REQUEST_LINES;
                turn.request_summary = request.first().cloned().unwrap_or_default();
                turn.request_lines = request.into_iter().take(REQUEST_LINES).collect();
                turn.request_full = full_lines(&text.join("\n"));
            }
        }
        if start.is_some() {
            previous = start;
        }
    }
}

pub fn turn_from_exchange(ex: &Exchange) -> TurnView {
    let tools = if ex.tools.is_empty() {
        ToolSummary::None
    } else {
        summarize_tools(ex.tools.iter().map(|t| t.name.as_str()), &ex.files)
    };
    build_turn(ex.position, &ex.timestamp, &ex.user, &ex.action, tools)
}

/// A row of `hcom transcript --json`. `None` when a required field has the
/// wrong type.
pub fn turn_from_remote_json(row: &Value) -> Option<TurnView> {
    let position = row.get("position")?.as_u64()? as usize;
    let user = row.get("user")?.as_str()?;
    let action = row.get("action")?.as_str()?;
    let timestamp = row.get("timestamp").and_then(Value::as_str).unwrap_or("");
    let files: Vec<String> = row
        .get("files")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|f| f.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let tools = match row.get("tools").and_then(Value::as_array) {
        None => ToolSummary::Unknown,
        Some(list) if list.is_empty() => ToolSummary::None,
        Some(list) => summarize_tools(
            list.iter()
                .filter_map(|t| t.get("name").and_then(Value::as_str)),
            &files,
        ),
    };
    Some(build_turn(position, timestamp, user, action, tools))
}

pub fn turns_from_remote_content(content: &str) -> Result<Vec<TurnView>, ()> {
    let rows: Vec<Value> = serde_json::from_str(content).map_err(|_| ())?;
    // One bad row fails the reply, so the last good conversation stays.
    rows.iter()
        .map(turn_from_remote_json)
        .collect::<Option<_>>()
        .ok_or(())
}

fn summarize_tools<'a>(names: impl Iterator<Item = &'a str>, files: &[String]) -> ToolSummary {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for name in names {
        *counts.entry(name.to_string()).or_default() += 1;
    }
    if counts.is_empty() {
        return ToolSummary::None;
    }
    let mut counts: Vec<(String, usize)> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    let mut unique: Vec<String> = Vec::new();
    for f in files {
        if !unique.contains(f) {
            unique.push(f.clone());
        }
    }
    let mut short = disambiguate_files(&unique);
    let extra_files = short.len().saturating_sub(MAX_FILES);
    short.truncate(MAX_FILES);
    ToolSummary::Some {
        counts,
        files: short,
        extra_files,
    }
}

fn split_path(path: &str) -> (Option<&str>, &str) {
    let trimmed = path.trim_end_matches(['/', '\\']);
    match trimmed.rfind(['/', '\\']) {
        Some(i) => {
            let parent = &trimmed[..i];
            let parent_base = parent.rsplit(['/', '\\']).next().filter(|s| !s.is_empty());
            (parent_base, &trimmed[i + 1..])
        }
        None => (None, trimmed),
    }
}

/// Basenames, with the parent directory kept where two basenames collide.
fn disambiguate_files(paths: &[String]) -> Vec<String> {
    let bases: Vec<&str> = paths.iter().map(|p| split_path(p).1).collect();
    paths
        .iter()
        .zip(&bases)
        .map(|(p, base)| {
            let clash = bases.iter().filter(|b| *b == base).count() > 1;
            match split_path(p).0 {
                Some(parent) if clash => format!("{parent}/{base}"),
                _ => base.to_string(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ex(user: &str, action: &str, tools: &[&str], files: &[&str]) -> Exchange {
        Exchange {
            usage: None,
            position: 1,
            user: user.into(),
            action: action.into(),
            final_text: String::new(),
            files: files.iter().map(|s| s.to_string()).collect(),
            timestamp: "2026-10-22T16:02:00Z".into(),
            tools: tools
                .iter()
                .map(|n| crate::transcript::ToolUse {
                    name: n.to_string(),
                    is_error: false,
                    file: None,
                    command: None,
                    output: None,
                })
                .collect(),
            edits: vec![],
            errors: vec![],
            ended_on_error: false,
        }
    }

    #[test]
    fn tools_counted_and_files_shortened() {
        let t = turn_from_exchange(&ex(
            "do it",
            "done",
            &["Read", "Edit", "Read", "Bash", "Read", "Edit"],
            &["src/a.rs", "tests/a.rs", "b.rs", "c.rs", "d.rs", "b.rs"],
        ));
        assert_eq!(
            t.tools,
            ToolSummary::Some {
                counts: vec![("Read".into(), 3), ("Edit".into(), 2), ("Bash".into(), 1)],
                files: vec!["src/a.rs".into(), "tests/a.rs".into(), "b.rs".into()],
                extra_files: 2,
            }
        );
    }

    #[test]
    fn long_response_keeps_tail() {
        let action: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let t = turn_from_exchange(&ex("q", &action, &[], &[]));
        assert_eq!(t.response_omitted, 12);
        assert_eq!(t.response_tail.len(), 8);
        assert_eq!(t.response_tail[0], "line 13");
        assert_eq!(t.response_summary, "line 20");
        assert_eq!(t.tools, ToolSummary::None);
    }

    #[test]
    fn compact_lines_keep_indentation() {
        let t = turn_from_exchange(&ex(
            "q",
            "code:\n\n    let x = 1;  \n  - nested\n",
            &[],
            &[],
        ));
        assert_eq!(
            t.response_tail,
            vec!["code:", "    let x = 1;", "  - nested"]
        );
        assert_eq!(t.response_summary, "- nested");
    }

    #[test]
    fn request_lines_capped() {
        let t = turn_from_exchange(&ex("a\n\nb\nc\nd", "", &[], &[]));
        assert_eq!(t.request_lines, vec!["a", "b", "c"]);
        assert!(t.request_more);
        assert_eq!(t.request_summary, "a");
        assert!(t.response_tail.is_empty());
        assert_eq!(t.response_summary, "");
    }

    #[test]
    fn slash_command_request_and_full_text() {
        let t = turn_from_exchange(&ex(
            "<command-message>personal</command-message>\n<command-name>/personal</command-name>\n<command-args>one question</command-args>",
            "a\n\n\nb  \n\n",
            &[],
            &[],
        ));
        assert_eq!(t.request_summary, "/personal one question");
        assert_eq!(t.request_full, vec!["/personal one question"]);
        assert_eq!(t.response_full, vec!["a", "", "b"]);
        assert_eq!(clean_request("plain"), "plain");
        assert_eq!(
            clean_request("<command-name>/clear</command-name><command-args></command-args>"),
            "/clear"
        );
    }

    fn msg(sender: &str, to: &[&str], body: &str, time: f64) -> Message {
        Message {
            event_id: 1,
            sender: sender.into(),
            recipients: to.iter().map(|s| s.to_string()).collect(),
            body: body.into(),
            time,
            sender_kind: crate::tui::model::SenderKind::Instance,
            intent: None,
            reply_to: None,
        }
    }

    #[test]
    fn wake_request_shows_received_messages() {
        let mut first = turn_from_exchange(&ex("q", "a", &[], &[]));
        first.started_at = Some(100.0);
        let mut wake = turn_from_exchange(&ex("<hcom>", "a", &[], &[]));
        assert!(wake.hcom_wake);
        wake.started_at = Some(200.0);
        let mut turns = vec![first, wake];
        let messages = vec![
            msg("bigboss", &["lava:TUGA"], "too early", 90.0),
            msg("bigboss", &["lava:TUGA"], "how long left?", 199.5),
            msg("luna", &["nova"], "not for lava", 199.0),
            msg("lava:TUGA", &[], "own broadcast", 199.0),
            msg("nova", &[], "all hands\nline 2", 201.0),
            msg("bigboss", &["lava:TUGA"], "too late", 210.0),
        ];
        fill_wake_requests(&mut turns, "lava:TUGA", &messages);
        assert_eq!(turns[0].request_summary, "q");
        assert_eq!(
            turns[1].request_full,
            vec!["bigboss: how long left?", "nova: all hands", "line 2"]
        );
        assert_eq!(turns[1].request_summary, "bigboss: how long left?");

        // Nothing found: the trigger stays.
        let mut lone = vec![turns[1].clone()];
        lone[0].request_full = vec!["<hcom>".into()];
        fill_wake_requests(&mut lone, "gigi", &messages[..1]);
        assert_eq!(lone[0].request_full, vec!["<hcom>"]);
    }

    #[test]
    fn claude_wrapped_requests() {
        let task = "<task-notification>\n<task-id>a1</task-id>\n<status>completed</status>\n\
                    <summary>Agent \"planner\" finished</summary>\n<result>done\nok</result>\n\
                    <usage><tool_uses>9</tool_uses></usage>\n</task-notification>";
        assert_eq!(
            clean_request(task),
            "[task] Agent \"planner\" finished\ndone\nok"
        );
        assert_eq!(
            clean_request("<task-notification><status>failed</status></task-notification>"),
            "[task] failed\n"
        );
        assert_eq!(
            clean_request("<bash-input>echo $HOME</bash-input>"),
            "! echo $HOME"
        );
        assert_eq!(
            clean_request(
                "<bash-stdout>(Bash completed with no output)</bash-stdout><bash-stderr></bash-stderr>"
            ),
            "(Bash completed with no output)"
        );
        assert_eq!(
            clean_request(
                "<local-command-caveat>Caveat text.</local-command-caveat>\n\
                 <local-command-stdout>Login successful</local-command-stdout>"
            ),
            "Login successful"
        );
        assert_eq!(
            clean_request("<local-command-caveat>only</local-command-caveat>"),
            ""
        );
        assert_eq!(strip_block("a<x>1</x>b<x>2</x>c<x>open", "x"), "abc<x>open");
        // Tags quoted inside a request are the user's text.
        let quoted = "why does <command-name>/clear</command-name> show up?";
        assert_eq!(clean_request(quoted), quoted);
        assert_eq!(
            clean_request("see <bash-stdout>x</bash-stdout>"),
            "see <bash-stdout>x</bash-stdout>"
        );
        assert_eq!(clean_request("  <bash-input>ls</bash-input>"), "! ls");
    }

    #[test]
    fn remote_rows() {
        let row =
            json!({"position": 3, "user": "hi", "action": "yo", "files": [], "timestamp": ""});
        assert_eq!(
            turn_from_remote_json(&row).unwrap().tools,
            ToolSummary::Unknown
        );
        let row = json!({"position": 3, "user": "hi", "action": "yo", "tools": []});
        assert_eq!(
            turn_from_remote_json(&row).unwrap().tools,
            ToolSummary::None
        );
        let row = json!({"position": 3, "user": "hi", "action": "yo", "files": ["x/y.rs"],
                         "tools": [{"name": "Edit"}, {"bad": 1}]});
        assert_eq!(
            turn_from_remote_json(&row).unwrap().tools,
            ToolSummary::Some {
                counts: vec![("Edit".into(), 1)],
                files: vec!["y.rs".into()],
                extra_files: 0
            }
        );
        let bad = json!({"position": "3", "user": "hi", "action": "yo"});
        assert!(turn_from_remote_json(&bad).is_none());
        assert!(turns_from_remote_content("not json").is_err());
        assert!(
            turns_from_remote_content(
                r#"[{"position":1,"user":"a","action":"b"},{"position":"2","user":"a","action":"b"}]"#
            )
            .is_err(),
            "one bad row fails the whole reply"
        );
        assert_eq!(
            turns_from_remote_content(r#"[{"position":1,"user":"a","action":"b"}]"#)
                .unwrap()
                .len(),
            1
        );
    }
}
