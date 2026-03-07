//! Resume command: `hcom r <name> [tool-args...]`
//!
//!
//! Loads a stopped instance's snapshot and relaunches with --resume session_id.

use anyhow::{Result, bail};
use serde_json::json;

use crate::db::HcomDb;
use crate::hooks::claude_args;
use crate::identity;
use crate::launcher::{self, LaunchParams};
use crate::log::log_info;
use crate::router::GlobalFlags;
use crate::tools::{codex_args, gemini_args};

/// Run the resume command. `argv` is the full argv[1..].
pub fn run(argv: &[String], flags: &GlobalFlags) -> Result<i32> {
    let (name, extra_args) = parse_resume_argv(argv, "r")?;

    do_resume(&name, false, &extra_args, flags)
}

/// Parse resume/fork argv: `r|f <name> [extra-args...]`
pub fn parse_resume_argv(argv: &[String], cmd: &str) -> Result<(String, Vec<String>)> {
    let mut i = 0;

    // Skip command name and global flags
    while i < argv.len() {
        match argv[i].as_str() {
            s if s == cmd || s == "resume" || s == "fork" || s == "f" => { i += 1; }
            "--name" => { i += 2; }
            "--go" => { i += 1; }
            _ => break,
        }
    }

    if i >= argv.len() {
        bail!("Usage: hcom {} <name> [tool-args...]", cmd);
    }

    let name = argv[i].clone();
    let extra_args = argv[i + 1..].to_vec();

    Ok((name, extra_args))
}

/// Core resume/fork logic.
pub fn do_resume(
    name: &str,
    fork: bool,
    extra_args: &[String],
    flags: &GlobalFlags,
) -> Result<i32> {
    let db = HcomDb::open()?;

    // For resume (not fork): reject if instance is still active
    if !fork {
        if let Ok(Some(_)) = db.get_instance_full(name) {
            bail!("'{}' is still active — run hcom stop {} first", name, name);
        }
    }

    // Load snapshot: from active instance (fork) or stopped event (resume)
    let (tool, session_id, launch_args_str, tag, background, last_event_id, snapshot_dir) = if fork {
        load_instance_data(&db, name)?
    } else {
        load_stopped_snapshot(&db, name)?
    };

    if session_id.is_empty() {
        bail!("No session ID found for '{}' — cannot {}", name, if fork { "fork" } else { "resume" });
    }

    // Extract hcom-level flags (--tag, --terminal, --dir) from extra args before tool parsing
    let (tag_override, terminal_override, dir_override, clean_extra) = extract_hcom_flags(extra_args);

    // Determine effective working directory:
    // - Explicit --dir flag wins (validated and canonicalized)
    // - For resume: use snapshot directory (continue where you left off)
    // - For fork: use current directory (start fresh in new context)
    let effective_cwd = if let Some(ref dir) = dir_override {
        let path = std::path::Path::new(dir);
        if !path.is_dir() {
            bail!("--dir path does not exist or is not a directory: {}", dir);
        }
        path.canonicalize()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| dir.clone())
    } else if fork {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string())
    } else if !snapshot_dir.is_empty() && std::path::Path::new(&snapshot_dir).is_dir() {
        snapshot_dir.clone()
    } else {
        if !snapshot_dir.is_empty() {
            eprintln!(
                "Warning: original directory '{}' no longer exists, using current directory",
                snapshot_dir
            );
        }
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string())
    };

    // Build tool-specific resume args
    let mut tool_args = build_resume_args(&tool, &session_id, fork);

    // Append cleaned extra args (without --tag/--terminal)
    tool_args.extend(clean_extra.into_iter());

    // Merge with original launch args
    let original_args: Vec<String> = if !launch_args_str.is_empty() {
        serde_json::from_str(&launch_args_str).unwrap_or_default()
    } else {
        Vec::new()
    };

    // For resume, merge original args with new args (new overrides)
    let merged_args = if !original_args.is_empty() {
        merge_resume_args(&tool, &original_args, &tool_args)
    } else {
        tool_args
    };

    // Detect headless
    let is_headless = is_headless_from_args(&tool, &merged_args) || background;
    let use_pty = tool == "claude" && !is_headless && cfg!(unix);

    // Resolve launcher name: explicit --name flag > identity > "user"
    let launcher_name = flags.name.clone().unwrap_or_else(|| {
        identity::resolve_identity(
            &db, None, None, None,
            std::env::var("HCOM_PROCESS_ID").ok().as_deref(),
            None, None,
        )
        .map(|id| id.name)
        .unwrap_or_else(|_| "user".to_string())
    });

    // Launch
    let result = launcher::launch(
        &db,
        LaunchParams {
            tool: tool.clone(),
            count: 1,
            args: merged_args,
            tag: tag_override.or(if tag.is_empty() { None } else { Some(tag) }),
            system_prompt: Some(if fork {
                format!(
                    "YOU ARE A FORK of agent '{}'. \
                     You have the same session history but are a NEW agent. \
                     Run hcom start to get your own identity.",
                    name
                )
            } else {
                format!("YOUR SESSION HAS BEEN RESUMED! You are still '{}'.", name)
            }),
            pty: use_pty,
            background: is_headless,
            cwd: Some(effective_cwd),
            env: None,
            launcher: Some(launcher_name),
            run_here: None,
            initial_prompt: None,
            batch_id: None,
            name: if fork { None } else { Some(name.to_string()) },
            skip_validation: true,
            terminal: terminal_override,
        },
    )?;

    // For resume: restore cursor so pending messages are delivered
    if !fork && last_event_id > 0 {
        crate::instances::update_instance_position(
            &db,
            name,
            &serde_json::Map::from_iter([(
                "last_event_id".to_string(),
                json!(last_event_id),
            )]),
        );
    }

    if result.launched > 0 {
        let action = if fork { "Forked" } else { "Resumed" };
        println!("{} {} ({})", action, name, tool);
    }

    log_info(
        if fork { "fork" } else { "resume" },
        &format!("cmd.{}", if fork { "fork" } else { "resume" }),
        &format!("name={} tool={} session={} launched={}", name, tool, session_id, result.launched),
    );

    Ok(if result.launched > 0 { 0 } else { 1 })
}

/// Extract hcom-level flags (--tag, --terminal, --name, --go) from args.
/// Returns (tag, terminal, remaining) with hcom flags stripped.
fn extract_hcom_flags(args: &[String]) -> (Option<String>, Option<String>, Option<String>, Vec<String>) {
    let mut tag = None;
    let mut terminal = None;
    let mut dir = None;
    let mut remaining = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--tag" && i + 1 < args.len() {
            tag = Some(args[i + 1].clone());
            i += 2;
        } else if args[i].starts_with("--tag=") {
            tag = Some(args[i][6..].to_string());
            i += 1;
        } else if args[i] == "--terminal" && i + 1 < args.len() {
            terminal = Some(args[i + 1].clone());
            i += 2;
        } else if args[i].starts_with("--terminal=") {
            terminal = Some(args[i][11..].to_string());
            i += 1;
        } else if args[i] == "--dir" && i + 1 < args.len() {
            dir = Some(args[i + 1].clone());
            i += 2;
        } else if args[i].starts_with("--dir=") {
            dir = Some(args[i][6..].to_string());
            i += 1;
        } else if args[i] == "--name" && i + 1 < args.len() {
            // --name is a global hcom flag, strip it so it doesn't leak to tool CLI
            i += 2;
        } else if args[i] == "--go" {
            i += 1;
        } else {
            remaining.push(args[i].clone());
            i += 1;
        }
    }
    (tag, terminal, dir, remaining)
}

/// Load data from an active or stopped instance.
fn load_instance_data(db: &HcomDb, name: &str) -> Result<(String, String, String, String, bool, i64, String)> {
    // Try active instance first
    if let Ok(Some(inst)) = db.get_instance_full(name) {
        return Ok((
            inst.tool.clone(),
            inst.session_id.as_deref().unwrap_or("").to_string(),
            inst.launch_args.as_deref().unwrap_or("").to_string(),
            inst.tag.as_deref().unwrap_or("").to_string(),
            inst.background != 0,
            inst.last_event_id,
            inst.directory.clone(),
        ));
    }

    // Fall back to stopped snapshot
    load_stopped_snapshot(db, name)
}

/// Load stopped snapshot from life events.
fn load_stopped_snapshot(db: &HcomDb, name: &str) -> Result<(String, String, String, String, bool, i64, String)> {
    // Query the latest "stopped" life event for this instance
    let mut stmt = db.conn().prepare(
        "SELECT data FROM events WHERE type='life' AND instance=? ORDER BY id DESC LIMIT 10",
    )?;

    let rows: Vec<String> = stmt
        .query_map(rusqlite::params![name], |row| row.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();

    for data_str in &rows {
        if let Ok(data) = serde_json::from_str::<serde_json::Value>(data_str) {
            if data.get("action").and_then(|v| v.as_str()) == Some("stopped") {
                if let Some(snapshot) = data.get("snapshot") {
                    let tool = snapshot.get("tool").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let session_id = snapshot.get("session_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let launch_args = snapshot.get("launch_args").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let tag = snapshot.get("tag").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let background = snapshot.get("background").and_then(|v| v.as_i64()).unwrap_or(0) != 0;
                    let last_event_id = snapshot.get("last_event_id").and_then(|v| v.as_i64()).unwrap_or(0);
                    let directory = snapshot.get("directory").and_then(|v| v.as_str()).unwrap_or("").to_string();

                    return Ok((tool, session_id, launch_args, tag, background, last_event_id, directory));
                }
            }
        }
    }

    bail!("No stopped snapshot found for '{}'", name)
}

/// Build tool-specific resume/fork args.
fn build_resume_args(tool: &str, session_id: &str, fork: bool) -> Vec<String> {
    match tool {
        "claude" | "claude-pty" => {
            let mut args = vec!["--resume".to_string(), session_id.to_string()];
            if fork {
                args.push("--fork-session".to_string());
            }
            args
        }
        "gemini" => {
            vec!["--resume".to_string(), session_id.to_string()]
        }
        "codex" => {
            let subcmd = if fork { "fork" } else { "resume" };
            vec![subcmd.to_string(), session_id.to_string()]
        }
        "opencode" => {
            let mut args = vec!["--session".to_string(), session_id.to_string()];
            if fork {
                args.push("--fork".to_string());
            }
            args
        }
        _ => Vec::new(),
    }
}

/// Merge original launch args with resume-specific args.
fn merge_resume_args(tool: &str, original: &[String], resume: &[String]) -> Vec<String> {
    // Resume args take precedence. We strip --resume/--session from original
    // and prepend resume args.
    match tool {
        "claude" | "claude-pty" => {
            let orig_spec = claude_args::resolve_claude_args(Some(original), None);
            let resume_spec = claude_args::resolve_claude_args(Some(resume), None);
            let merged = claude_args::merge_claude_args(&orig_spec, &resume_spec);
            merged.rebuild_tokens(true)
        }
        "gemini" => {
            let orig_spec = gemini_args::resolve_gemini_args(Some(original), None);
            let resume_spec = gemini_args::resolve_gemini_args(Some(resume), None);
            let merged = gemini_args::merge_gemini_args(&orig_spec, &resume_spec);
            merged.rebuild_tokens(true, true)
        }
        "codex" => {
            let orig_spec = codex_args::resolve_codex_args(Some(original), None);
            let resume_spec = codex_args::resolve_codex_args(Some(resume), None);
            let merged = codex_args::merge_codex_args(&orig_spec, &resume_spec);
            merged.rebuild_tokens(true, true)
        }
        _ => {
            // For opencode and unknown: resume args only
            resume.to_vec()
        }
    }
}

/// Check if args indicate headless mode.
fn is_headless_from_args(tool: &str, args: &[String]) -> bool {
    match tool {
        "claude" | "claude-pty" => {
            let spec = claude_args::resolve_claude_args(Some(args), None);
            spec.is_background
        }
        "gemini" => {
            let spec = gemini_args::resolve_gemini_args(Some(args), None);
            spec.is_headless
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|i| i.to_string()).collect()
    }

    #[test]
    fn test_parse_resume_argv() {
        let (name, extra) = parse_resume_argv(&s(&["r", "luna"]), "r").unwrap();
        assert_eq!(name, "luna");
        assert!(extra.is_empty());
    }

    #[test]
    fn test_parse_resume_argv_with_extra() {
        let (name, extra) = parse_resume_argv(&s(&["r", "luna", "--model", "opus"]), "r").unwrap();
        assert_eq!(name, "luna");
        assert_eq!(extra, s(&["--model", "opus"]));
    }

    #[test]
    fn test_parse_resume_argv_empty_fails() {
        assert!(parse_resume_argv(&s(&["r"]), "r").is_err());
    }

    #[test]
    fn test_build_resume_args_claude() {
        let args = build_resume_args("claude", "sess-123", false);
        assert_eq!(args, s(&["--resume", "sess-123"]));
    }

    #[test]
    fn test_build_resume_args_claude_fork() {
        let args = build_resume_args("claude", "sess-123", true);
        assert_eq!(args, s(&["--resume", "sess-123", "--fork-session"]));
    }

    #[test]
    fn test_build_resume_args_codex_resume() {
        let args = build_resume_args("codex", "sess-456", false);
        assert_eq!(args, s(&["resume", "sess-456"]));
    }

    #[test]
    fn test_build_resume_args_codex_fork() {
        let args = build_resume_args("codex", "sess-456", true);
        assert_eq!(args, s(&["fork", "sess-456"]));
    }

    #[test]
    fn test_build_resume_args_gemini() {
        let args = build_resume_args("gemini", "sess-789", false);
        assert_eq!(args, s(&["--resume", "sess-789"]));
    }

    #[test]
    fn test_build_resume_args_opencode_fork() {
        let args = build_resume_args("opencode", "sess-000", true);
        assert_eq!(args, s(&["--session", "sess-000", "--fork"]));
    }

    #[test]
    fn test_extract_hcom_flags_terminal() {
        let (tag, terminal, dir, remaining) = extract_hcom_flags(&s(&["--terminal", "alacritty", "--model", "opus"]));
        assert_eq!(tag, None);
        assert_eq!(terminal, Some("alacritty".to_string()));
        assert_eq!(dir, None);
        assert_eq!(remaining, s(&["--model", "opus"]));
    }

    #[test]
    fn test_extract_hcom_flags_tag_and_terminal() {
        let (tag, terminal, dir, remaining) = extract_hcom_flags(&s(&["--tag", "test", "--terminal", "kitty"]));
        assert_eq!(tag, Some("test".to_string()));
        assert_eq!(terminal, Some("kitty".to_string()));
        assert_eq!(dir, None);
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_extract_hcom_flags_equals_form() {
        let (tag, terminal, dir, remaining) = extract_hcom_flags(&s(&["--tag=test", "--terminal=alacritty"]));
        assert_eq!(tag, Some("test".to_string()));
        assert_eq!(terminal, Some("alacritty".to_string()));
        assert_eq!(dir, None);
        assert!(remaining.is_empty());
    }

    #[test]
    fn test_extract_hcom_flags_none() {
        let (tag, terminal, dir, remaining) = extract_hcom_flags(&s(&["--model", "opus"]));
        assert_eq!(tag, None);
        assert_eq!(terminal, None);
        assert_eq!(dir, None);
        assert_eq!(remaining, s(&["--model", "opus"]));
    }

    #[test]
    fn test_extract_hcom_flags_dir() {
        let (tag, terminal, dir, remaining) = extract_hcom_flags(&s(&["--dir", "/tmp/test", "--model", "opus"]));
        assert_eq!(tag, None);
        assert_eq!(terminal, None);
        assert_eq!(dir, Some("/tmp/test".to_string()));
        assert_eq!(remaining, s(&["--model", "opus"]));
    }
}
