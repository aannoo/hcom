//! Gemini CLI hook handlers for hcom.
//!
//! Lifecycle: SessionStart → BeforeAgent → [BeforeTool → AfterTool]* → AfterAgent → SessionEnd

use std::env;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::Value;

use crate::db::{HcomDb, InstanceRow};
use crate::hooks::common;
use crate::hooks::{HookPayload, HookResult};
use crate::instance_binding;
use crate::instance_lifecycle as lifecycle;
use crate::instances;
use crate::log;
use crate::shared::constants::BIND_MARKER_RE;
use crate::shared::context::HcomContext;
use crate::shared::{ST_ACTIVE, ST_BLOCKED, ST_LISTENING};

/// Derive Gemini CLI transcript path from session_id.
///
/// Gemini's ChatRecordingService isn't initialized at SessionStart, so
/// transcript_path is empty. This derives it from session_id by searching
/// the Gemini chats directory.
///
pub fn derive_gemini_transcript_path(session_id: &str) -> Option<String> {
    if session_id.is_empty() {
        return None;
    }

    let session_prefix = session_id.split('-').next().unwrap_or("");
    if session_prefix.is_empty() {
        return None;
    }

    let home = env::var("HOME").ok()?;
    let gemini_tmp = PathBuf::from(&home).join(".gemini").join("tmp");
    if !gemini_tmp.exists() {
        return None;
    }

    // Search for session-*-{prefix}*.json recursively in chats/ dirs
    let pattern = format!("session-*-{}*.json", session_prefix);
    find_newest_matching_file(&gemini_tmp, &pattern)
}

/// Recursively search for files matching a glob pattern under chats/ directories.
/// Returns the most recently modified match.
fn find_newest_matching_file(base: &Path, pattern: &str) -> Option<String> {
    let mut best: Option<(String, std::time::SystemTime)> = None;
    let mut dirs_to_visit = vec![base.to_path_buf()];

    while let Some(dir) = dirs_to_visit.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            dirs_to_visit.push(path.clone());
            if path.file_name().is_some_and(|n| n == "chats") {
                check_chat_dir(&path, pattern, &mut best);
            }
        }
    }

    best.map(|(path, _)| path)
}

/// Check a chats/ directory for matching session files, updating `best` if newer.
fn check_chat_dir(
    chat_dir: &Path,
    pattern: &str,
    best: &mut Option<(String, std::time::SystemTime)>,
) {
    let entries = match std::fs::read_dir(chat_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !matches_session_pattern(name, pattern) {
            continue;
        }
        let mtime = match path.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let dominated = best.as_ref().is_some_and(|(_, bt)| mtime <= *bt);
        if !dominated {
            *best = Some((path.to_string_lossy().to_string(), mtime));
        }
    }
}

/// Check if a filename matches the session pattern "session-*-{prefix}*.json".
fn matches_session_pattern(filename: &str, pattern: &str) -> bool {
    // pattern is "session-*-{prefix}*.json"
    // Extract prefix from pattern
    let prefix = pattern
        .strip_prefix("session-*-")
        .and_then(|s| s.strip_suffix("*.json"))
        .unwrap_or("");

    filename.starts_with("session-") && filename.ends_with(".json") && filename.contains(prefix)
}

/// Try to capture transcript_path from payload if not already set.
///
/// Gemini's ChatRecordingService isn't initialized at SessionStart,
/// so transcript_path is empty. It becomes available at BeforeAgent/AfterAgent.
fn try_capture_transcript_path(db: &HcomDb, instance_name: &str, payload: &HookPayload) {
    let instance = match db.get_instance_full(instance_name) {
        Ok(Some(data)) => data,
        _ => return,
    };

    // Re-derive if stored path doesn't exist (e.g. after kill/resume, Gemini
    // creates a new transcript file with a different timestamp prefix).
    if !instance.transcript_path.is_empty()
        && std::path::Path::new(&instance.transcript_path).exists()
    {
        return;
    }

    let transcript_path = payload.transcript_path.clone().or_else(|| {
        let session_id = instance.session_id.as_deref().unwrap_or("");
        if session_id.is_empty() {
            None
        } else {
            let derived = derive_gemini_transcript_path(session_id).unwrap_or_default();
            if derived.is_empty() {
                None
            } else {
                Some(derived)
            }
        }
    });

    if let Some(tp) = transcript_path {
        let mut updates = serde_json::Map::new();
        updates.insert("transcript_path".into(), Value::String(tp));
        instances::update_instance_position(db, instance_name, &updates);
    }
}

/// Resolve instance using process binding or session binding.
fn resolve_instance_gemini(db: &HcomDb, payload: &HookPayload) -> Option<InstanceRow> {
    instance_binding::resolve_instance_from_binding(db, payload.session_id.as_deref(), None)
}

/// Bind vanilla Gemini instance by parsing tool_result for [hcom:X] marker.
fn bind_vanilla_instance(db: &HcomDb, payload: &HookPayload) -> Option<String> {
    // Skip if no pending instances (optimization)
    let pending = common::get_pending_instances(db);
    if pending.is_empty() {
        return None;
    }

    // Only check run_shell_command tool responses
    if payload.tool_name != "run_shell_command" {
        return None;
    }

    let tool_response = &payload.tool_result;
    if tool_response.is_empty() {
        return None;
    }

    let caps = BIND_MARKER_RE.captures(tool_response)?;
    let instance_name = caps[1].to_string();

    super::family::bind_vanilla_instance(
        db,
        &instance_name,
        payload.session_id.as_deref(),
        payload.transcript_path.as_deref(),
        "gemini",
        "gemini-aftertool",
    )
}

/// Handle Gemini SessionStart hook.
///
/// HCOM-launched: bind session_id, inject bootstrap if not announced.
/// Vanilla: show hcom hint.
fn handle_sessionstart(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    if ctx.process_id.is_none() {
        // Vanilla instance - show hint
        return HookResult::Allow {
            additional_context: Some(format!(
                "[hcom available - run '{} start' to participate]",
                crate::runtime_env::build_hcom_command()
            )),
            system_message: None,
        };
    }

    let session_id = match payload.session_id.as_deref() {
        Some(sid) => sid,
        None => return hook_noop(),
    };

    let instance_name =
        instance_binding::bind_session_to_process(db, session_id, ctx.process_id.as_deref());

    log::log_info(
        "hooks",
        "gemini.sessionstart.bind",
        &format!(
            "instance={:?} session_id={} process_id={:?}",
            instance_name, session_id, ctx.process_id,
        ),
    );

    // Orphaned PTY: process_id exists but no binding (e.g., after session clear)
    let instance_name = match instance_name {
        Some(name) => name,
        None => {
            if let Some(ref pid) = ctx.process_id {
                match instance_binding::create_orphaned_pty_identity(
                    db,
                    session_id,
                    Some(pid.as_str()),
                    "gemini",
                ) {
                    Some(name) => {
                        log::log_info(
                            "hooks",
                            "gemini.sessionstart.orphan_created",
                            &format!("instance={} process_id={}", name, pid),
                        );
                        name
                    }
                    None => return hook_noop(),
                }
            } else {
                return hook_noop();
            }
        }
    };

    let _ = db.rebind_instance_session(&instance_name, session_id);

    // Capture launch context
    instance_binding::capture_and_store_launch_context(db, &instance_name);

    let mut updates = serde_json::Map::new();
    updates.insert(
        "directory".into(),
        Value::String(ctx.cwd.to_string_lossy().to_string()),
    );
    if let Some(ref tp) = payload.transcript_path {
        updates.insert("transcript_path".into(), Value::String(tp.clone()));
    }
    instances::update_instance_position(db, &instance_name, &updates);
    lifecycle::set_status(
        db,
        &instance_name,
        ST_LISTENING,
        "start",
        Default::default(),
    );

    crate::runtime_env::set_terminal_title(&instance_name);

    // Auto-spawn relay-worker now that an instance is active
    crate::relay::worker::ensure_worker(true);

    // Bootstrap injection moved to BeforeAgent only
    // Reason: Gemini doesn't display SessionStart hook output after /clear
    hook_noop()
}

/// Handle BeforeAgent hook - fires after user submits prompt.
///
/// Fallback bootstrap if SessionStart injection failed.
/// Also delivers pending messages and binds session_id for fresh instances.
fn handle_beforeagent(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_instance_gemini(db, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };

    let instance_name = &instance.name;

    // Keep directory current
    let mut dir_updates = serde_json::Map::new();
    dir_updates.insert(
        "directory".into(),
        Value::String(ctx.cwd.to_string_lossy().to_string()),
    );
    instances::update_instance_position(db, instance_name, &dir_updates);

    // Bind session_id if instance doesn't have one (fresh instance after /clear)
    if instance.session_id.is_none() {
        if let Some(ref sid) = payload.session_id {
            log::log_info(
                "hooks",
                "gemini.beforeagent.bind_session",
                &format!("instance={} session_id={}", instance_name, sid),
            );
            let mut sid_updates = serde_json::Map::new();
            sid_updates.insert("session_id".into(), Value::String(sid.clone()));
            instances::update_instance_position(db, instance_name, &sid_updates);
            if let Err(e) = db.rebind_session(sid, instance_name) {
                log::log_warn(
                    "hooks",
                    "gemini.rebind_failed",
                    &format!("rebind_session failed for {instance_name}: {e}"),
                );
            }
            if let Some(ref pid) = ctx.process_id {
                if let Err(e) = db.set_process_binding(pid, sid, instance_name) {
                    log::log_warn(
                        "hooks",
                        "gemini.process_binding_failed",
                        &format!("set_process_binding failed for {instance_name}: {e}"),
                    );
                }
            }
        }
    }

    try_capture_transcript_path(db, instance_name, payload);

    let mut outputs: Vec<String> = Vec::new();

    // Inject bootstrap if not already announced
    if let Some(bootstrap) =
        common::inject_bootstrap_once(db, ctx, instance_name, &instance, "gemini")
    {
        outputs.push(bootstrap);
    }

    // Deliver pending messages
    let (_msgs, formatted) = common::deliver_pending_messages(db, instance_name);
    if let Some(formatted) = formatted {
        outputs.push(formatted);
    } else {
        // Real user prompt (not hcom injection)
        lifecycle::set_status(db, instance_name, ST_ACTIVE, "prompt", Default::default());
    }

    if outputs.is_empty() {
        return hook_noop();
    }

    let combined = outputs.join("\n\n---\n\n");
    HookResult::Allow {
        additional_context: Some(combined),
        system_message: None,
    }
}

/// Handle AfterAgent hook - fires when agent turn completes.
fn handle_afteragent(db: &HcomDb, _ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_instance_gemini(db, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };

    lifecycle::set_status(db, &instance.name, ST_LISTENING, "", Default::default());
    common::notify_hook_instance_with_db(db, &instance.name);

    hook_noop()
}

/// Handle BeforeTool hook - fires before tool execution.
fn handle_beforetool(db: &HcomDb, _ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_instance_gemini(db, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };

    let tool_name = if payload.tool_name.is_empty() {
        "unknown"
    } else {
        &payload.tool_name
    };
    common::update_tool_status(db, &instance.name, "gemini", tool_name, &payload.tool_input);

    hook_noop()
}

/// Handle AfterTool hook - fires after tool execution.
///
/// Vanilla binding: detects [hcom:X] marker from hcom start output.
/// Bootstrap injection and message delivery via additionalContext.
fn handle_aftertool(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let mut instance: Option<InstanceRow> = None;

    // Vanilla binding: try tool_response first (immediate)
    if ctx.process_id.is_none() {
        if let Some(bound_name) = bind_vanilla_instance(db, payload) {
            instance = db.get_instance_full(&bound_name).ok().flatten();
        }
    }

    // Process/session binding fallback
    if instance.is_none() {
        instance = resolve_instance_gemini(db, payload);
    }

    let instance = match instance {
        Some(inst) => inst,
        None => return hook_noop(),
    };

    let instance_name = &instance.name;
    let mut outputs: Vec<String> = Vec::new();

    // Inject bootstrap if not already announced
    if let Some(bootstrap) =
        common::inject_bootstrap_once(db, ctx, instance_name, &instance, "gemini")
    {
        outputs.push(bootstrap);
    }

    // Deliver pending messages (JSON format)
    let (_msgs, formatted) = common::deliver_pending_messages(db, instance_name);
    if let Some(formatted) = formatted {
        outputs.push(formatted);
    }

    if outputs.is_empty() {
        return hook_noop();
    }

    let combined = outputs.join("\n\n---\n\n");
    HookResult::Allow {
        additional_context: Some(combined),
        system_message: None,
    }
}

/// Handle Notification hook - fires on approval prompts, etc.
fn handle_notification(db: &HcomDb, _ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_instance_gemini(db, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };

    if payload.notification_type.as_deref() == Some("ToolPermission") {
        lifecycle::set_status(
            db,
            &instance.name,
            ST_BLOCKED,
            "approval",
            Default::default(),
        );
    }

    hook_noop()
}

/// Handle SessionEnd hook - fires when a session ends.
fn handle_sessionend(db: &HcomDb, _ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_instance_gemini(db, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };

    let reason = payload
        .raw
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    common::finalize_session(db, &instance.name, reason, None);

    hook_noop()
}

/// No-op hook result: allow with no additional context.
fn hook_noop() -> HookResult {
    HookResult::Allow {
        additional_context: None,
        system_message: None,
    }
}

/// Gemini hook handler name → function dispatch.
fn get_handler(hook_name: &str) -> Option<fn(&HcomDb, &HcomContext, &HookPayload) -> HookResult> {
    match hook_name {
        "gemini-sessionstart" => Some(handle_sessionstart),
        "gemini-beforeagent" => Some(handle_beforeagent),
        "gemini-afteragent" => Some(handle_afteragent),
        "gemini-beforetool" => Some(handle_beforetool),
        "gemini-aftertool" => Some(handle_aftertool),
        "gemini-notification" => Some(handle_notification),
        "gemini-sessionend" => Some(handle_sessionend),
        _ => None,
    }
}

/// Main entry point for Gemini hooks — called by router.
///
/// Reads stdin JSON, builds HookPayload + HcomContext, dispatches to handler.
/// Prints JSON to stdout (additionalContext for Gemini to inject).
///
pub fn dispatch_gemini_hook(hook_name: &str) -> i32 {
    let start = Instant::now();

    // Build context from environment
    let ctx = HcomContext::from_os();

    // Read stdin JSON
    let stdin_json: Value = match serde_json::from_reader(std::io::stdin().lock()) {
        Ok(v) => v,
        Err(_) => Value::Object(Default::default()),
    };

    // Build payload
    let payload = HookPayload::from_gemini(stdin_json);

    // Pre-gate: skip BeforeAgent for non-participants
    if !ctx.is_launched && hook_name == "gemini-beforeagent" {
        let sid = match payload.session_id.as_deref() {
            Some(sid) => sid,
            None => return 0,
        };
        // Quick DB check for session binding
        if let Ok(db) = HcomDb::open() {
            if db.get_session_binding(sid).ok().flatten().is_none() {
                return 0;
            }
        } else {
            return 0;
        }
    }

    // Ensure hcom directories exist
    let init_start = Instant::now();
    if !crate::paths::ensure_hcom_directories() {
        return 0;
    }
    let init_ms = init_start.elapsed().as_secs_f64() * 1000.0;

    // Open DB (includes schema migration/compat check)
    let db = match HcomDb::open() {
        Ok(db) => db,
        Err(e) => {
            log::log_error("hooks", "gemini.db.error", &format!("{}", e));
            return 0;
        }
    };

    // Pre-gate: non-participants with empty DB → exit 0, no output
    if !common::hook_gate_check(&ctx, &db) {
        return 0;
    }

    // Get handler
    let handler = match get_handler(hook_name) {
        Some(h) => h,
        None => {
            log::log_error(
                "hooks",
                "gemini.dispatch.unknown",
                &format!("Unknown Gemini hook: {}", hook_name),
            );
            return 0;
        }
    };

    // Execute handler
    let handler_start = Instant::now();
    let result = common::dispatch_with_panic_guard(
        "gemini",
        hook_name,
        HookResult::Allow {
            additional_context: None,
            system_message: None,
        },
        || handler(&db, &ctx, &payload),
    );

    let handler_ms = handler_start.elapsed().as_secs_f64() * 1000.0;
    let total_ms = start.elapsed().as_secs_f64() * 1000.0;

    log::log_info(
        "hooks",
        "gemini.dispatch.timing",
        &format!(
            "hook={} init_ms={:.2} handler_ms={:.2} total_ms={:.2} exit_code={}",
            hook_name,
            init_ms,
            handler_ms,
            total_ms,
            result.exit_code()
        ),
    );

    // Output result JSON to stdout — Gemini expects hookSpecificOutput wrapper
    let exit_code = result.exit_code();
    let output_json = match &result {
        // Note: system_message on Allow is unused for Gemini (not part of Gemini hook schema)
        HookResult::Allow {
            additional_context, ..
        } => {
            if let Some(ctx) = additional_context {
                // Map hook name to Gemini event name (e.g. "gemini-beforeagent" → "BeforeAgent")
                let event_name = match hook_name {
                    "gemini-sessionstart" => "SessionStart",
                    "gemini-beforeagent" => "BeforeAgent",
                    "gemini-afteragent" => "AfterAgent",
                    "gemini-beforetool" => "BeforeTool",
                    "gemini-aftertool" => "AfterTool",
                    "gemini-notification" => "Notification",
                    "gemini-sessionend" => "SessionEnd",
                    _ => hook_name,
                };
                Some(serde_json::json!({
                    "decision": "allow",
                    "hookSpecificOutput": {
                        "hookEventName": event_name,
                        "additionalContext": ctx,
                    }
                }))
            } else {
                None
            }
        }
        HookResult::Block { reason } => Some(serde_json::json!({
            "decision": "block",
            "reason": reason,
        })),
        HookResult::UpdateInput { updated_input } => {
            Some(serde_json::json!({ "updatedInput": updated_input }))
        }
    };
    if let Some(json) = output_json {
        let _ = serde_json::to_writer(std::io::stdout().lock(), &json);
    }

    exit_code
}

/// Find an executable in PATH.
fn find_in_path(name: &str) -> Option<PathBuf> {
    crate::terminal::which_bin(name).map(PathBuf::from)
}

/// Minimum supported Gemini CLI version (hooksConfig.enabled schema).
pub const GEMINI_MIN_VERSION: (u32, u32, u32) = (0, 26, 0);

/// Get installed Gemini CLI version without subprocess.
///
/// Resolves the gemini binary symlink and reads version from package.json.
/// Returns (major, minor, patch) or None if not found/parseable.
///
pub fn get_gemini_version() -> Option<(u32, u32, u32)> {
    let gemini_path = find_in_path("gemini")?;
    let real_path = std::fs::canonicalize(&gemini_path).ok()?;

    // package.json is in same dir as dist/ for npm installs
    let mut package_json = real_path.parent()?.join("package.json");
    if !package_json.exists() {
        // Try parent (dist/index.js -> package.json at package root)
        package_json = real_path.parent()?.parent()?.join("package.json");
    }
    if !package_json.exists() {
        return None;
    }

    let content = std::fs::read_to_string(&package_json).ok()?;
    let data: serde_json::Value = serde_json::from_str(&content).ok()?;
    let version_str = data.get("version")?.as_str()?;
    let parts: Vec<&str> = version_str.split('.').collect();
    if parts.len() >= 3 {
        let major = parts[0].parse().ok()?;
        let minor = parts[1].parse().ok()?;
        // Handle versions like "0.24.0-beta.1"
        let patch_str = parts[2].split('-').next()?;
        let patch = patch_str.parse().ok()?;
        Some((major, minor, patch))
    } else {
        None
    }
}

/// Check if installed Gemini version supports hcom hooks (>= 0.26.0).
///
/// Returns True if version detected and >= 0.26.0, or if version can't be
/// detected (optimistic fallback). False only if version detected AND too old.
pub fn is_gemini_version_supported() -> bool {
    match get_gemini_version() {
        Some(v) => v >= GEMINI_MIN_VERSION,
        None => true, // Can't determine — allow optimistically
    }
}

/// Safe hcom commands for Gemini auto-approval permission patterns.
use super::common::SAFE_HCOM_COMMANDS;

/// Hook configuration: (hook_type, matcher, command_suffix, timeout, description).
const GEMINI_HOOK_CONFIGS: &[(&str, &str, &str, u32, &str)] = &[
    (
        "SessionStart",
        "*",
        "gemini-sessionstart",
        5000,
        "Connect to hcom network",
    ),
    (
        "BeforeAgent",
        "*",
        "gemini-beforeagent",
        5000,
        "Deliver pending messages",
    ),
    (
        "AfterAgent",
        "*",
        "gemini-afteragent",
        5000,
        "Signal ready for messages",
    ),
    (
        "BeforeTool",
        ".*",
        "gemini-beforetool",
        5000,
        "Track tool execution",
    ),
    (
        "AfterTool",
        ".*",
        "gemini-aftertool",
        5000,
        "Deliver messages after tools",
    ),
    (
        "Notification",
        "ToolPermission",
        "gemini-notification",
        5000,
        "Track approval prompts",
    ),
    (
        "SessionEnd",
        "*",
        "gemini-sessionend",
        5000,
        "Disconnect from hcom",
    ),
];

/// Build all legacy permission patterns (both hcom and uvx hcom) for removal from tools.allowed.
fn build_all_permission_patterns() -> Vec<String> {
    let mut patterns = Vec::new();
    for prefix in &["hcom", "uvx hcom"] {
        for cmd in SAFE_HCOM_COMMANDS {
            patterns.push(format!("run_shell_command({} {})", prefix, cmd));
        }
    }
    patterns
}

/// Get path to Gemini policies directory.
///
/// If HCOM_DIR is set (sandbox), uses HCOM_DIR parent.
/// Otherwise uses global (~/.gemini/policies/).
fn get_gemini_policies_path() -> PathBuf {
    crate::runtime_env::tool_config_root()
        .join(".gemini")
        .join("policies")
}

/// Build policy TOML content for hcom.toml.
///
/// Uses commandPrefix array to allow all safe hcom commands in a single rule.
/// Matches the Codex pattern of a separate, self-contained permission file.
fn build_gemini_policy() -> String {
    let prefix = crate::runtime_env::build_hcom_command();
    let command_prefixes: Vec<String> = SAFE_HCOM_COMMANDS
        .iter()
        .map(|cmd| format!("  \"{} {}\"", prefix, cmd))
        .collect();

    format!(
        "# hcom integration - auto-approve safe commands\n\
         [[rule]]\n\
         toolName = \"run_shell_command\"\n\
         commandPrefix = [\n\
         {},\n\
         ]\n\
         decision = \"allow\"\n\
         priority = 300\n",
        command_prefixes.join(",\n")
    )
}

/// Set up Gemini policy file for auto-approval.
fn setup_gemini_policy() -> bool {
    let policies_dir = get_gemini_policies_path();
    let policy_file = policies_dir.join("hcom.toml");
    let policy_content = build_gemini_policy();

    // Check if already configured correctly
    if policy_file.exists() {
        if let Ok(existing) = std::fs::read_to_string(&policy_file) {
            if existing == policy_content {
                return true;
            }
        }
    }

    let _ = std::fs::create_dir_all(&policies_dir);
    crate::paths::atomic_write(&policy_file, &policy_content)
}

/// Remove hcom policy file.
fn remove_gemini_policy() -> bool {
    let policy_file = get_gemini_policies_path().join("hcom.toml");
    if policy_file.exists() {
        std::fs::remove_file(&policy_file).is_ok()
    } else {
        true
    }
}

/// Remove policy from a specific policies directory path.
fn remove_policy_from_path(policies_dir: &Path) -> bool {
    let policy_file = policies_dir.join("hcom.toml");
    if policy_file.exists() {
        std::fs::remove_file(&policy_file).is_ok()
    } else {
        true
    }
}

/// Get path to Gemini settings file.
///
/// If HCOM_DIR is set (sandbox), uses HCOM_DIR parent.
/// Otherwise uses global (~/.gemini/settings.json).
pub fn get_gemini_settings_path() -> PathBuf {
    crate::runtime_env::tool_config_root()
        .join(".gemini")
        .join("settings.json")
}

/// Load Gemini settings from JSON file.
fn load_gemini_settings(path: &Path) -> Option<serde_json::Map<String, Value>> {
    let content = std::fs::read_to_string(path).ok()?;
    let val: Value = serde_json::from_str(&content).ok()?;
    val.as_object().cloned()
}

/// Check if a hook dict is an hcom hook.
fn is_hcom_hook(hook: &Value) -> bool {
    let command = hook.get("command").and_then(|v| v.as_str()).unwrap_or("");
    let name = hook.get("name").and_then(|v| v.as_str()).unwrap_or("");
    // Check for hcom-related patterns
    command.contains("hcom")
        || name.contains("hcom-")
        || command.contains("${HCOM")
        || command.contains("$HCOM")
}

/// Set hooksConfig.enabled = true and clean up legacy hooks.enabled.
fn set_hooks_enabled(settings: &mut serde_json::Map<String, Value>) {
    // Ensure hooksConfig exists and set enabled
    if !settings.contains_key("hooksConfig") {
        settings.insert("hooksConfig".into(), serde_json::json!({}));
    }
    if let Some(hc) = settings
        .get_mut("hooksConfig")
        .and_then(|v| v.as_object_mut())
    {
        hc.insert("enabled".into(), Value::Bool(true));
    }

    // Clean up legacy hooks.enabled
    if let Some(hooks) = settings.get_mut("hooks").and_then(|v| v.as_object_mut()) {
        if hooks.get("enabled").and_then(|v| v.as_bool()).is_some() {
            hooks.remove("enabled");
            if hooks.is_empty() {
                settings.remove("hooks");
            }
        }
    }
}

/// Check if hooksConfig.enabled is set.
fn is_hooks_enabled(settings: &serde_json::Map<String, Value>) -> bool {
    settings
        .get("hooksConfig")
        .and_then(|v| v.get("enabled"))
        .and_then(|v| v.as_bool())
        == Some(true)
}

/// Remove hcom hooks from Gemini settings dict (in-place).
///
/// Only removes hcom-specific hooks, preserving user hooks.
fn remove_hcom_hooks_from_settings(settings: &mut serde_json::Map<String, Value>) {
    if let Some(hooks_val) = settings.get_mut("hooks") {
        if let Some(hooks) = hooks_val.as_object_mut() {
            let hook_types: Vec<String> = hooks.keys().cloned().collect();
            for hook_type in hook_types {
                if let Some(matchers) = hooks.get_mut(&hook_type).and_then(|v| v.as_array_mut()) {
                    let mut updated = Vec::new();
                    for matcher in matchers.iter() {
                        if let Some(matcher_obj) = matcher.as_object() {
                            if let Some(hook_list) =
                                matcher_obj.get("hooks").and_then(|v| v.as_array())
                            {
                                let non_hcom: Vec<Value> = hook_list
                                    .iter()
                                    .filter(|h| !is_hcom_hook(h))
                                    .cloned()
                                    .collect();
                                if !non_hcom.is_empty() {
                                    let mut new_matcher = matcher_obj.clone();
                                    new_matcher.insert("hooks".into(), Value::Array(non_hcom));
                                    updated.push(Value::Object(new_matcher));
                                } else if !matcher_obj.contains_key("hooks") {
                                    updated.push(matcher.clone());
                                }
                                // else: had only hcom hooks — drop
                            } else {
                                updated.push(matcher.clone());
                            }
                        } else {
                            updated.push(matcher.clone());
                        }
                    }
                    if updated.is_empty() {
                        hooks.remove(&hook_type);
                    } else {
                        hooks.insert(hook_type, Value::Array(updated));
                    }
                }
            }

            // Clean up legacy hooks.enabled
            if hooks.get("enabled").and_then(|v| v.as_bool()).is_some() {
                hooks.remove("enabled");
            }

            if hooks.is_empty() {
                settings.remove("hooks");
            }
        }
    }

    // Remove hcom permission patterns from tools.allowed
    if let Some(tools) = settings.get_mut("tools").and_then(|v| v.as_object_mut()) {
        if let Some(allowed) = tools.get_mut("allowed").and_then(|v| v.as_array_mut()) {
            let all_patterns = build_all_permission_patterns();
            allowed.retain(|v| {
                v.as_str()
                    .map(|s| !all_patterns.iter().any(|p| p == s))
                    .unwrap_or(true)
            });
            if allowed.is_empty() {
                tools.remove("allowed");
            }
        }
    }
}

/// Ensure hooksConfig.enabled = true, migrating from legacy hooks.enabled if needed.
///
/// Call this on any hcom gemini command to auto-fix settings.
/// Skips mutation if Gemini version < 0.26.0.
pub fn ensure_hooks_enabled() -> bool {
    let version = get_gemini_version();
    if let Some(v) = version {
        if v < GEMINI_MIN_VERSION {
            return false;
        }
    }

    let settings_path = get_gemini_settings_path();
    if !settings_path.exists() {
        return true; // setup_gemini_hooks will handle it
    }

    let mut settings = match load_gemini_settings(&settings_path) {
        Some(s) => s,
        None => serde_json::Map::new(),
    };

    let needs_migration = settings
        .get("hooks")
        .and_then(|v| v.get("enabled"))
        .and_then(|v| v.as_bool())
        .is_some();

    if is_hooks_enabled(&settings) && !needs_migration {
        return true;
    }

    set_hooks_enabled(&mut settings);

    let json_str = serde_json::to_string_pretty(&Value::Object(settings)).unwrap_or_default();
    crate::paths::atomic_write(&settings_path, &json_str)
}

/// Set up hcom hooks in Gemini settings.json.
///
/// - Removes existing hcom hooks first (clean slate)
/// - Adds all hooks from GEMINI_HOOK_CONFIGS
/// - Uses atomic write for safety
///
pub fn setup_gemini_hooks(include_permissions: bool) -> bool {
    // Guard: block only if version detected AND too old
    let version = get_gemini_version();
    if let Some(v) = version {
        if v < GEMINI_MIN_VERSION {
            return false;
        }
    }

    let settings_path = get_gemini_settings_path();
    if let Some(parent) = settings_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let mut settings = load_gemini_settings(&settings_path).unwrap_or_default();

    // Remove existing hcom hooks (clean slate)
    remove_hcom_hooks_from_settings(&mut settings);

    // Ensure tools.enableHooks = true
    if !settings.contains_key("tools") {
        settings.insert("tools".into(), serde_json::json!({}));
    }
    if let Some(tools) = settings.get_mut("tools").and_then(|v| v.as_object_mut()) {
        tools.insert("enableHooks".into(), Value::Bool(true));

        // Clean up legacy tools.allowed entries (migrated to policy engine)
        if let Some(allowed) = tools.get_mut("allowed").and_then(|v| v.as_array_mut()) {
            let all_patterns = build_all_permission_patterns();
            allowed.retain(|v| {
                v.as_str()
                    .map(|s| !all_patterns.iter().any(|p| p == s))
                    .unwrap_or(true)
            });
            if allowed.is_empty() {
                tools.remove("allowed");
            }
        }
    }

    // Handle permissions via policy engine (~/.gemini/policies/hcom.toml)
    if include_permissions {
        setup_gemini_policy();
    } else {
        remove_gemini_policy();
    }

    let hcom_cmd = crate::runtime_env::build_hcom_command();

    // Set hooksConfig.enabled
    set_hooks_enabled(&mut settings);

    // Ensure hooks dict exists
    if !settings.contains_key("hooks") || !settings["hooks"].is_object() {
        settings.insert("hooks".into(), serde_json::json!({}));
    }

    // Add hook entries
    if let Some(hooks) = settings.get_mut("hooks").and_then(|v| v.as_object_mut()) {
        for &(hook_type, matcher, cmd_suffix, timeout, description) in GEMINI_HOOK_CONFIGS {
            let hook_name = format!("hcom-{}", hook_type.to_lowercase());
            let hook_entry = serde_json::json!({
                "matcher": matcher,
                "hooks": [{
                    "name": hook_name,
                    "type": "command",
                    "command": format!("{} {}", hcom_cmd, cmd_suffix),
                    "timeout": timeout,
                    "description": description,
                }]
            });

            if !hooks.contains_key(hook_type) || !hooks[hook_type].is_array() {
                hooks.insert(hook_type.into(), Value::Array(Vec::new()));
            }
            if let Some(arr) = hooks.get_mut(hook_type).and_then(|v| v.as_array_mut()) {
                arr.push(hook_entry);
            }
        }
    }

    let json_str = serde_json::to_string_pretty(&Value::Object(settings)).unwrap_or_default();
    if !crate::paths::atomic_write(&settings_path, &json_str) {
        return false;
    }

    verify_gemini_hooks_installed(include_permissions)
}

/// Verify that hcom hooks are correctly installed in Gemini settings.
///
/// Checks enableHooks, hooksConfig.enabled, all hook types present,
/// correct command, and optionally permissions.
pub fn verify_gemini_hooks_installed(check_permissions: bool) -> bool {
    verify_hooks_at(&get_gemini_settings_path(), check_permissions)
}

fn verify_hooks_at(settings_path: &Path, check_permissions: bool) -> bool {
    let settings = match load_gemini_settings(settings_path) {
        Some(s) if !s.is_empty() => s,
        _ => return false,
    };

    // Check tools.enableHooks or legacy enableHooks
    let enable_hooks = settings
        .get("tools")
        .and_then(|v| v.get("enableHooks"))
        .and_then(|v| v.as_bool())
        .or_else(|| settings.get("enableHooks").and_then(|v| v.as_bool()));
    if enable_hooks != Some(true) {
        return false;
    }

    // Check hooksConfig.enabled
    if !is_hooks_enabled(&settings) {
        return false;
    }

    // Check all hook types
    let hooks = match settings.get("hooks").and_then(|v| v.as_object()) {
        Some(h) => h,
        None => return false,
    };

    for &(hook_type, expected_matcher, cmd_suffix, expected_timeout, _) in GEMINI_HOOK_CONFIGS {
        let hook_matchers = match hooks.get(hook_type).and_then(|v| v.as_array()) {
            Some(arr) if !arr.is_empty() => arr,
            _ => return false,
        };

        let expected_name = format!("hcom-{}", hook_type.to_lowercase());
        let mut found = false;

        for matcher_dict in hook_matchers {
            let matcher_obj = match matcher_dict.as_object() {
                Some(o) => o,
                None => continue,
            };

            let actual_matcher = matcher_obj
                .get("matcher")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let matcher_hooks = match matcher_obj.get("hooks").and_then(|v| v.as_array()) {
                Some(h) => h,
                None => continue,
            };

            for hook in matcher_hooks {
                if is_hcom_hook(hook) {
                    if found {
                        return false; // Duplicate
                    }
                    if actual_matcher != expected_matcher {
                        return false;
                    }
                    if hook.get("type").and_then(|v| v.as_str()) != Some("command") {
                        return false;
                    }
                    if hook.get("name").and_then(|v| v.as_str()) != Some(&expected_name) {
                        return false;
                    }
                    if hook.get("timeout").and_then(|v| v.as_u64()) != Some(expected_timeout as u64)
                    {
                        return false;
                    }
                    let command = hook.get("command").and_then(|v| v.as_str()).unwrap_or("");
                    let has_hcom = command.contains("${HCOM}")
                        || command.to_ascii_lowercase().contains("hcom");
                    if !has_hcom || !command.contains(cmd_suffix) {
                        return false;
                    }
                    found = true;
                }
            }
        }

        if !found {
            return false;
        }
    }

    // Check permissions via policy engine
    if check_permissions {
        let policy_file = get_gemini_policies_path().join("hcom.toml");
        if !policy_file.exists() {
            return false;
        }
    }

    true
}

/// Remove hcom hooks from Gemini settings (global + local).
///
/// Removes hooks from settings.json and policy file from policies/.
pub fn remove_gemini_hooks() -> bool {
    let global_path = dirs::home_dir()
        .map(|h| h.join(".gemini").join("settings.json"))
        .unwrap_or_default();
    let local_path = get_gemini_settings_path();

    let global_ok = remove_hooks_from_path(&global_path);
    let local_ok = if local_path != global_path {
        remove_hooks_from_path(&local_path)
    } else {
        true
    };

    // Remove policy files
    let global_policies = dirs::home_dir()
        .map(|h| h.join(".gemini").join("policies"))
        .unwrap_or_default();
    let local_policies = get_gemini_policies_path();

    let global_policy_ok = remove_policy_from_path(&global_policies);
    let local_policy_ok = if local_policies != global_policies {
        remove_policy_from_path(&local_policies)
    } else {
        true
    };

    global_ok && local_ok && global_policy_ok && local_policy_ok
}

fn remove_hooks_from_path(path: &Path) -> bool {
    if !path.exists() {
        return true;
    }
    let mut settings = match load_gemini_settings(path) {
        Some(s) => s,
        None => return true,
    };

    remove_hcom_hooks_from_settings(&mut settings);

    let json_str = serde_json::to_string_pretty(&Value::Object(settings)).unwrap_or_default();
    crate::paths::atomic_write(path, &json_str)
}

/// Gemini arg parser result — detects headless mode and validates conflicts.
///
/// Full arg parser (merge, update, rebuild) is future scope.
#[derive(Debug, Clone)]
pub struct GeminiArgsSpec {
    pub is_headless: bool,
    pub has_prompt_flag: bool,
    pub has_prompt_interactive: bool,
    pub is_yolo: bool,
    pub errors: Vec<String>,
}

/// Boolean flags (no value required).
const GEMINI_BOOLEAN_FLAGS: &[&str] = &[
    "-d",
    "--debug",
    "-s",
    "--sandbox",
    "-y",
    "--yolo",
    "-l",
    "--list-extensions",
    "--list-sessions",
    "--screen-reader",
    "-v",
    "--version",
    "-h",
    "--help",
    "--experimental-acp",
    "--raw-output",
    "--accept-raw-output-risk",
];

/// Flags that require a following value.
const GEMINI_VALUE_FLAGS: &[&str] = &[
    "-m",
    "--model",
    "-p",
    "--prompt",
    "-i",
    "--prompt-interactive",
    "--approval-mode",
    "--allowed-mcp-server-names",
    "--allowed-tools",
    "-e",
    "--extensions",
    "--delete-session",
    "--include-directories",
    "-o",
    "--output-format",
];

/// Optional value flags (can be used with or without a value).
const GEMINI_OPTIONAL_VALUE_FLAGS: &[&str] = &["--resume", "-r"];

/// Known subcommands.
const GEMINI_SUBCOMMANDS: &[&str] = &[
    "mcp",
    "extensions",
    "extension",
    "hooks",
    "hook",
    "skills",
    "skill",
];

/// Parse Gemini CLI args to detect headless mode and validate conflicts.
///
pub fn parse_gemini_args(args: &[String]) -> GeminiArgsSpec {
    let mut is_headless = false;
    let mut has_prompt_flag = false;
    let mut has_prompt_interactive = false;
    let mut is_yolo = false;
    let mut errors = Vec::new();
    let mut has_positional = false;

    let mut i = 0;

    // Skip subcommand if present
    if !args.is_empty() {
        let first = args[0].to_lowercase();
        if GEMINI_SUBCOMMANDS.contains(&first.as_str()) {
            i = 1;
        }
    }

    let mut pending_flag: Option<String> = None;
    let mut after_double_dash = false;

    while i < args.len() {
        let token = &args[i];
        let token_lower = token.to_lowercase();

        // Handle pending value flag
        if let Some(ref flag) = pending_flag {
            let flag_lower = flag.to_lowercase();
            if flag_lower == "-p" || flag_lower == "--prompt" {
                has_prompt_flag = true;
                is_headless = true;
            }
            if flag_lower == "-i" || flag_lower == "--prompt-interactive" {
                has_prompt_interactive = true;
            }
            if flag_lower == "--approval-mode" && token_lower == "yolo" {
                is_yolo = true;
            }
            pending_flag = None;
            i += 1;
            continue;
        }

        // After -- separator
        if after_double_dash {
            has_positional = true;
            if !has_prompt_interactive {
                is_headless = true;
            }
            i += 1;
            continue;
        }

        if token == "--" {
            after_double_dash = true;
            i += 1;
            continue;
        }

        // Boolean flags
        if GEMINI_BOOLEAN_FLAGS.contains(&token_lower.as_str()) {
            if token_lower == "--yolo" || token_lower == "-y" {
                is_yolo = true;
            }
            i += 1;
            continue;
        }

        // Optional value flags (--resume)
        if GEMINI_OPTIONAL_VALUE_FLAGS.contains(&token_lower.as_str()) {
            i += 1;
            // Peek: consume value only if looks like session ID
            if i < args.len() {
                let next = &args[i];
                if !next.starts_with('-') && looks_like_session_id(next) {
                    i += 1;
                }
            }
            continue;
        }

        // --flag=value syntax
        let mut matched_eq = false;
        for flag in GEMINI_VALUE_FLAGS
            .iter()
            .chain(GEMINI_OPTIONAL_VALUE_FLAGS.iter())
        {
            let prefix = format!("{}=", flag);
            if token_lower.starts_with(&prefix) {
                let value = &token[prefix.len()..];
                let flag_lower = flag.to_lowercase();
                if flag_lower == "-p" || flag_lower == "--prompt" {
                    has_prompt_flag = true;
                    is_headless = true;
                }
                if flag_lower == "-i" || flag_lower == "--prompt-interactive" {
                    has_prompt_interactive = true;
                }
                if flag_lower == "--approval-mode" && value.to_lowercase() == "yolo" {
                    is_yolo = true;
                }
                matched_eq = true;
                break;
            }
        }
        if matched_eq {
            i += 1;
            continue;
        }

        // Value flags (space-separated)
        if GEMINI_VALUE_FLAGS.contains(&token_lower.as_str()) {
            pending_flag = Some(token.clone());
            i += 1;
            continue;
        }

        // Positional or unknown
        if !token_lower.starts_with('-') {
            has_positional = true;
            if !has_prompt_interactive {
                is_headless = true;
            }
        }
        i += 1;
    }

    // Validate conflicts
    if has_positional {
        errors.push(
            "ERROR: Gemini headless mode (positional query) not supported in hcom.\n\
             Use -i/--prompt-interactive for interactive sessions with initial prompt.\n\
             For headless: use 'hcom N claude -p \"task\"'"
                .to_string(),
        );
    } else if has_prompt_flag {
        errors.push(
            "ERROR: Gemini headless mode (-p/--prompt flag) not supported in hcom.\n\
             Use -i/--prompt-interactive for interactive sessions with initial prompt.\n\
             For headless: use 'hcom N claude -p \"task\"'"
                .to_string(),
        );
    }

    if is_yolo && has_prompt_flag {
        // --yolo with --approval-mode together check
    }

    GeminiArgsSpec {
        is_headless,
        has_prompt_flag,
        has_prompt_interactive,
        is_yolo,
        errors,
    }
}

/// Check if token looks like a Gemini session ID (numeric, "latest", or UUID).
fn looks_like_session_id(token: &str) -> bool {
    let lower = token.to_lowercase();
    if lower == "latest" {
        return true;
    }
    if token.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    // UUID: 8-4-4-4-12 hex chars
    let parts: Vec<&str> = lower.split('-').collect();
    parts.len() == 5
        && parts[0].len() == 8
        && parts[1].len() == 4
        && parts[2].len() == 4
        && parts[3].len() == 4
        && parts[4].len() == 12
        && lower.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_matches_session_pattern() {
        assert!(matches_session_pattern(
            "session-1-abc123-uuid-here.json",
            "session-*-abc123*.json"
        ));
        assert!(matches_session_pattern(
            "session-42-abc123.json",
            "session-*-abc123*.json"
        ));
        assert!(!matches_session_pattern(
            "session-1-xyz999.json",
            "session-*-abc123*.json"
        ));
        assert!(!matches_session_pattern(
            "other-file.txt",
            "session-*-abc123*.json"
        ));
    }

    #[test]
    fn test_derive_gemini_transcript_path_empty() {
        assert!(derive_gemini_transcript_path("").is_none());
    }

    #[test]
    fn test_derive_gemini_transcript_path_no_panic() {
        // Non-existent session prefix — must not panic (returns None or Some depending on fs state)
        let _ = derive_gemini_transcript_path("nonexistent-uuid-12345678");
    }

    #[test]
    fn test_get_handler_known() {
        assert!(get_handler("gemini-sessionstart").is_some());
        assert!(get_handler("gemini-beforeagent").is_some());
        assert!(get_handler("gemini-afteragent").is_some());
        assert!(get_handler("gemini-beforetool").is_some());
        assert!(get_handler("gemini-aftertool").is_some());
        assert!(get_handler("gemini-notification").is_some());
        assert!(get_handler("gemini-sessionend").is_some());
    }

    #[test]
    fn test_get_handler_unknown() {
        assert!(get_handler("gemini-unknown").is_none());
        assert!(get_handler("sessionstart").is_none());
    }

    #[test]
    fn test_hook_noop() {
        let result = hook_noop();
        assert_eq!(result.exit_code(), 0);
        match &result {
            HookResult::Allow {
                additional_context,
                system_message,
            } => {
                assert!(additional_context.is_none());
                assert!(system_message.is_none());
            }
            _ => panic!("expected Allow"),
        }
    }

    #[test]
    fn test_hook_payload_gemini_tool_result() {
        // Test dict format with llmContent
        let raw = serde_json::json!({
            "session_id": "gem-1",
            "tool_response": {"llmContent": "command output here"},
            "tool_name": "run_shell_command"
        });
        let payload = HookPayload::from_gemini(raw);
        assert_eq!(payload.tool_result, "command output here");
        assert_eq!(payload.tool_name, "run_shell_command");

        // Test dict format with output
        let raw2 = serde_json::json!({
            "session_id": "gem-2",
            "tool_response": {"output": "other output"}
        });
        let payload2 = HookPayload::from_gemini(raw2);
        assert_eq!(payload2.tool_result, "other output");

        // Test no tool_response
        let raw3 = serde_json::json!({"session_id": "gem-3"});
        let payload3 = HookPayload::from_gemini(raw3);
        assert_eq!(payload3.tool_result, "");
    }

    #[test]
    fn test_hook_payload_gemini_notification_type() {
        let raw = serde_json::json!({
            "session_id": "gem-1",
            "notification_type": "ToolPermission"
        });
        let payload = HookPayload::from_gemini(raw);
        assert_eq!(payload.notification_type.as_deref(), Some("ToolPermission"));
    }

    #[test]
    fn test_hook_payload_gemini_tool_name_variants() {
        // Test toolName field (camelCase fallback)
        let raw = serde_json::json!({
            "session_id": "gem-1",
            "toolName": "run_shell_command"
        });
        let payload = HookPayload::from_gemini(raw);
        assert_eq!(payload.tool_name, "run_shell_command");

        // Test tool_name field (snake_case — primary format)
        let raw2 = serde_json::json!({
            "session_id": "gem-2",
            "tool_name": "write_file"
        });
        let payload2 = HookPayload::from_gemini(raw2);
        assert_eq!(payload2.tool_name, "write_file");

        // Test missing tool_name → empty
        let raw3 = serde_json::json!({
            "session_id": "gem-3"
        });
        let payload3 = HookPayload::from_gemini(raw3);
        assert_eq!(payload3.tool_name, "");
    }

    #[test]
    fn test_hook_payload_gemini_tool_input_variants() {
        // Test tool_input (snake_case — primary format)
        let raw = serde_json::json!({
            "session_id": "gem-1",
            "tool_input": {"command": "ls"}
        });
        let payload = HookPayload::from_gemini(raw);
        assert_eq!(payload.tool_input["command"], "ls");

        // Test toolInput (camelCase fallback)
        let raw2 = serde_json::json!({
            "session_id": "gem-2",
            "toolInput": {"file_path": "/tmp/test"}
        });
        let payload2 = HookPayload::from_gemini(raw2);
        assert_eq!(payload2.tool_input["file_path"], "/tmp/test");

        // Test missing tool_input → empty object
        let raw3 = serde_json::json!({
            "session_id": "gem-3"
        });
        let payload3 = HookPayload::from_gemini(raw3);
        assert!(payload3.tool_input.is_object());
    }

    #[test]
    fn test_is_hcom_hook() {
        let hcom_hook = serde_json::json!({
            "name": "hcom-sessionstart",
            "type": "command",
            "command": "hcom gemini-sessionstart"
        });
        assert!(is_hcom_hook(&hcom_hook));

        let user_hook = serde_json::json!({
            "name": "my-hook",
            "type": "command",
            "command": "/usr/local/bin/my-script"
        });
        assert!(!is_hcom_hook(&user_hook));
    }

    #[test]
    fn test_set_hooks_enabled() {
        let mut settings = serde_json::Map::new();
        set_hooks_enabled(&mut settings);
        assert!(is_hooks_enabled(&settings));
    }

    #[test]
    fn test_set_hooks_enabled_migrates_legacy() {
        let mut settings = serde_json::Map::new();
        let mut hooks = serde_json::Map::new();
        hooks.insert("enabled".into(), Value::Bool(true));
        hooks.insert("SessionStart".into(), serde_json::json!([]));
        settings.insert("hooks".into(), Value::Object(hooks));

        set_hooks_enabled(&mut settings);

        // hooksConfig.enabled should be true
        assert!(is_hooks_enabled(&settings));
        // Legacy hooks.enabled should be removed
        let hooks = settings.get("hooks").and_then(|v| v.as_object()).unwrap();
        assert!(hooks.get("enabled").is_none());
    }

    #[test]
    fn test_remove_hcom_hooks_preserves_user_hooks() {
        let mut settings: serde_json::Map<String, Value> = serde_json::from_value(serde_json::json!({
            "hooks": {
                "SessionStart": [{
                    "matcher": "*",
                    "hooks": [
                        {"name": "hcom-sessionstart", "type": "command", "command": "hcom gemini-sessionstart"},
                        {"name": "my-hook", "type": "command", "command": "/usr/bin/my-script"}
                    ]
                }]
            }
        })).unwrap();

        remove_hcom_hooks_from_settings(&mut settings);

        // User hook should be preserved
        let hooks = settings.get("hooks").unwrap();
        let ss = hooks.get("SessionStart").unwrap().as_array().unwrap();
        assert_eq!(ss.len(), 1);
        let remaining = ss[0].get("hooks").unwrap().as_array().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(
            remaining[0].get("name").unwrap().as_str().unwrap(),
            "my-hook"
        );
    }

    #[test]
    fn test_remove_hcom_hooks_drops_empty() {
        let mut settings: serde_json::Map<String, Value> = serde_json::from_value(serde_json::json!({
            "hooks": {
                "SessionStart": [{
                    "matcher": "*",
                    "hooks": [
                        {"name": "hcom-sessionstart", "type": "command", "command": "hcom gemini-sessionstart"}
                    ]
                }]
            }
        })).unwrap();

        remove_hcom_hooks_from_settings(&mut settings);

        // hooks dict should be removed (all empty)
        assert!(settings.get("hooks").is_none());
    }

    #[test]
    fn test_build_gemini_policy() {
        let policy = build_gemini_policy();
        assert!(policy.contains("[[rule]]"));
        assert!(policy.contains("toolName = \"run_shell_command\""));
        assert!(policy.contains("decision = \"allow\""));
        assert!(policy.contains("priority = 300"));
        assert!(policy.contains("hcom send"));
        assert!(policy.contains("hcom list"));
        assert!(policy.contains("commandPrefix"));
    }

    #[test]
    #[serial]
    fn test_setup_and_verify_gemini_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let hcom_dir = dir.path().join(".hcom");
        std::fs::create_dir_all(&hcom_dir).unwrap();
        let settings_dir = dir.path().join(".gemini");
        std::fs::create_dir_all(&settings_dir).unwrap();

        // Redirect paths via HCOM_DIR
        let saved = std::env::var("HCOM_DIR").ok();
        unsafe { std::env::set_var("HCOM_DIR", &hcom_dir) };

        let success = setup_gemini_hooks(true);
        assert!(success, "setup should succeed");

        let verified = verify_gemini_hooks_installed(true);
        assert!(verified, "verify should pass after setup");

        // Check settings file was written
        let settings_path = dir.path().join(".gemini").join("settings.json");
        assert!(settings_path.exists());
        let content = std::fs::read_to_string(&settings_path).unwrap();
        assert!(content.contains("hcom-sessionstart"));
        assert!(content.contains("hcom-beforeagent"));
        assert!(content.contains("enableHooks"));

        // Check policy file was written
        let policy_path = dir
            .path()
            .join(".gemini")
            .join("policies")
            .join("hcom.toml");
        assert!(policy_path.exists(), "policy file should be created");

        // Remove hooks
        let remove_ok = remove_hooks_from_path(&settings_path);
        assert!(remove_ok);
        let verify_after_remove = verify_hooks_at(&settings_path, false);
        assert!(!verify_after_remove, "verify should fail after remove");

        // Restore
        if let Some(v) = saved {
            unsafe { std::env::set_var("HCOM_DIR", v) };
        } else {
            unsafe { std::env::remove_var("HCOM_DIR") };
        }
    }

    #[test]
    fn test_parse_gemini_args_no_args() {
        let spec = parse_gemini_args(&[]);
        assert!(!spec.is_headless);
        assert!(spec.errors.is_empty());
    }

    #[test]
    fn test_parse_gemini_args_headless_positional() {
        let args: Vec<String> = vec!["explain rust".into()];
        let spec = parse_gemini_args(&args);
        assert!(spec.is_headless);
        assert!(!spec.errors.is_empty());
        assert!(spec.errors[0].contains("headless"));
    }

    #[test]
    fn test_parse_gemini_args_headless_prompt_flag() {
        let args: Vec<String> = vec!["-p".into(), "write code".into()];
        let spec = parse_gemini_args(&args);
        assert!(spec.is_headless);
        assert!(spec.has_prompt_flag);
        assert!(!spec.errors.is_empty());
    }

    #[test]
    fn test_parse_gemini_args_interactive_not_headless() {
        let args: Vec<String> = vec!["-i".into(), "initial prompt".into()];
        let spec = parse_gemini_args(&args);
        assert!(!spec.is_headless);
        assert!(spec.has_prompt_interactive);
        assert!(spec.errors.is_empty());
    }

    #[test]
    fn test_parse_gemini_args_model_flag() {
        let args: Vec<String> = vec!["--model".into(), "gemini-2.0".into()];
        let spec = parse_gemini_args(&args);
        assert!(!spec.is_headless);
        assert!(spec.errors.is_empty());
    }

    #[test]
    fn test_parse_gemini_args_yolo() {
        let args: Vec<String> = vec!["--yolo".into()];
        let spec = parse_gemini_args(&args);
        assert!(spec.is_yolo);
    }

    #[test]
    fn test_parse_gemini_args_subcommand_skipped() {
        let args: Vec<String> = vec!["hooks".into(), "--help".into()];
        let spec = parse_gemini_args(&args);
        assert!(!spec.is_headless);
        assert!(spec.errors.is_empty());
    }

    #[test]
    fn test_parse_gemini_args_prompt_equals() {
        let args: Vec<String> = vec!["--prompt=write code".into()];
        let spec = parse_gemini_args(&args);
        assert!(spec.is_headless);
        assert!(spec.has_prompt_flag);
    }

    #[test]
    fn test_looks_like_session_id() {
        assert!(looks_like_session_id("latest"));
        assert!(looks_like_session_id("42"));
        assert!(looks_like_session_id(
            "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
        ));
        assert!(!looks_like_session_id("explain rust"));
        assert!(!looks_like_session_id("--model"));
    }

    use crate::hooks::test_helpers::{EnvGuard, isolated_test_env};
    use serial_test::serial;

    fn gemini_test_env() -> (tempfile::TempDir, PathBuf, PathBuf, EnvGuard) {
        let (dir, _hcom_dir, test_home, guard) = isolated_test_env();
        let settings_path = test_home.join(".gemini").join("settings.json");
        (dir, test_home, settings_path, guard)
    }

    /// Independent verification: check no hcom hooks present in settings JSON.
    fn independently_verify_no_hcom_hooks(settings: &Value) -> Vec<String> {
        let mut violations = Vec::new();
        let hooks = match settings.get("hooks").and_then(|v| v.as_object()) {
            Some(h) => h,
            None => return violations,
        };
        for (hook_type, matchers_val) in hooks {
            if hook_type == "enabled" || hook_type == "disabled" {
                continue;
            }
            let matchers = match matchers_val.as_array() {
                Some(a) => a,
                None => continue,
            };
            for (i, matcher) in matchers.iter().enumerate() {
                let hooks_arr = match matcher.get("hooks").and_then(|v| v.as_array()) {
                    Some(a) => a,
                    None => continue,
                };
                for (j, hook) in hooks_arr.iter().enumerate() {
                    let name = hook.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let command = hook.get("command").and_then(|v| v.as_str()).unwrap_or("");
                    if name.contains("hcom") || command.contains("hcom") {
                        violations.push(format!(
                            "{hook_type}[{i}].hooks[{j}]: name={name}, command={command}"
                        ));
                    }
                }
            }
        }
        violations
    }

    /// Independent verification: check expected hcom hooks are present.
    fn independently_verify_hcom_hooks_present(
        settings: &Value,
        expected: &[(&str, &str)], // (hook_type, cmd_suffix)
    ) -> Vec<String> {
        let hcom_cmd = crate::runtime_env::build_hcom_command();
        let mut missing = Vec::new();
        let hooks = match settings.get("hooks").and_then(|v| v.as_object()) {
            Some(h) => h,
            None => {
                return expected
                    .iter()
                    .map(|(ht, _)| format!("{ht}: hooks dict missing"))
                    .collect();
            }
        };
        for &(hook_type, cmd_suffix) in expected {
            let expected_full = format!("{} {}", hcom_cmd, cmd_suffix);
            let matchers = match hooks.get(hook_type).and_then(|v| v.as_array()) {
                Some(a) => a,
                None => {
                    missing.push(format!("{hook_type}: not present"));
                    continue;
                }
            };
            let mut found = false;
            for matcher in matchers {
                if let Some(hook_list) = matcher.get("hooks").and_then(|v| v.as_array()) {
                    for hook in hook_list {
                        if let Some(cmd) = hook.get("command").and_then(|v| v.as_str()) {
                            if cmd == expected_full {
                                found = true;
                                break;
                            }
                        }
                    }
                }
                if found {
                    break;
                }
            }
            if !found {
                missing.push(format!(
                    "{hook_type}: expected exact command '{expected_full}', not found"
                ));
            }
        }
        missing
    }

    fn read_json(path: &Path) -> Value {
        let content = std::fs::read_to_string(path).unwrap();
        serde_json::from_str(&content).unwrap()
    }

    #[test]
    #[serial]
    fn test_setup_gemini_hooks_installs_expected() {
        let (_dir, _test_home, settings_path, _guard) = gemini_test_env();

        let result = setup_gemini_hooks(false);
        assert!(result, "setup should succeed");
        assert!(settings_path.exists());

        let settings = read_json(&settings_path);

        // tools.enableHooks must be true
        assert_eq!(settings["tools"]["enableHooks"], true);

        // hooksConfig.enabled must be true (not legacy hooks.enabled)
        assert_eq!(
            settings
                .get("hooksConfig")
                .and_then(|v| v.get("enabled"))
                .and_then(|v| v.as_bool()),
            Some(true)
        );
        assert!(
            settings
                .get("hooks")
                .and_then(|v| v.get("enabled"))
                .is_none(),
            "legacy hooks.enabled should not be present"
        );

        // Each hook type from GEMINI_HOOK_CONFIGS must be present with correct values
        let hooks = settings.get("hooks").unwrap();
        for &(hook_type, expected_matcher, cmd_suffix, expected_timeout, _) in GEMINI_HOOK_CONFIGS {
            let arr = hooks
                .get(hook_type)
                .and_then(|v| v.as_array())
                .unwrap_or_else(|| panic!("{hook_type} missing or not array"));
            assert_eq!(arr.len(), 1, "{hook_type} should have 1 matcher");

            let matcher_dict = &arr[0];
            assert_eq!(
                matcher_dict
                    .get("matcher")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
                expected_matcher,
                "{hook_type} matcher mismatch"
            );

            let hook_list = matcher_dict
                .get("hooks")
                .and_then(|v| v.as_array())
                .unwrap();
            assert_eq!(hook_list.len(), 1, "{hook_type} should have 1 hook");

            let hook = &hook_list[0];
            assert_eq!(hook["type"], "command");
            assert_eq!(
                hook["name"].as_str().unwrap(),
                format!("hcom-{}", hook_type.to_lowercase())
            );
            assert_eq!(hook["timeout"].as_u64().unwrap(), expected_timeout as u64);
            let expected_command = format!(
                "{} {}",
                crate::runtime_env::build_hcom_command(),
                cmd_suffix
            );
            assert_eq!(
                hook["command"].as_str().unwrap(),
                expected_command,
                "{hook_type} command mismatch"
            );
        }

        assert!(verify_gemini_hooks_installed(false));

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_setup_gemini_preserves_user_model_settings() {
        let (_dir, _test_home, settings_path, _guard) = gemini_test_env();

        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        let user_settings = serde_json::json!({
            "model": {
                "name": "gemini-2.5-pro",
                "maxSessionTurns": 50,
                "compressionThreshold": 0.3,
            },
            "ui": {
                "hideBanner": true,
            }
        });
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&user_settings).unwrap(),
        )
        .unwrap();

        assert!(setup_gemini_hooks(false));

        let updated = read_json(&settings_path);
        assert_eq!(updated["model"]["name"], "gemini-2.5-pro");
        assert_eq!(updated["model"]["maxSessionTurns"], 50);
        assert_eq!(updated["model"]["compressionThreshold"], 0.3);
        assert_eq!(updated["ui"]["hideBanner"], true);

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_setup_gemini_preserves_user_tools_allowed_and_uses_policy() {
        let (_dir, test_home, settings_path, _guard) = gemini_test_env();

        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        let user_settings = serde_json::json!({
            "tools": {
                "allowed": ["run_shell_command(git status)", "read_file"]
            }
        });
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&user_settings).unwrap(),
        )
        .unwrap();

        assert!(setup_gemini_hooks(true));

        let updated = read_json(&settings_path);
        let allowed = updated["tools"]["allowed"].as_array().unwrap();
        assert!(
            allowed
                .iter()
                .any(|v| v.as_str() == Some("run_shell_command(git status)")),
            "user's allowed entry should be preserved"
        );
        assert!(
            allowed.iter().any(|v| v.as_str() == Some("read_file")),
            "user's read_file entry should be preserved"
        );
        // hcom permissions should NOT be in tools.allowed (moved to policy engine)
        assert!(
            !allowed
                .iter()
                .any(|v| v.as_str().map(|s| s.contains("hcom")).unwrap_or(false)),
            "hcom permissions should not be in tools.allowed"
        );

        // Policy file should exist instead
        let policy_file = test_home.join(".gemini").join("policies").join("hcom.toml");
        assert!(policy_file.exists(), "policy file should be created");
        let policy_content = std::fs::read_to_string(&policy_file).unwrap();
        assert!(policy_content.contains("hcom send"));
        assert!(policy_content.contains("decision = \"allow\""));

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_setup_gemini_idempotent() {
        let (_dir, _test_home, settings_path, _guard) = gemini_test_env();

        assert!(setup_gemini_hooks(false));
        let first = std::fs::read_to_string(&settings_path).unwrap();

        assert!(setup_gemini_hooks(false));
        let second = std::fs::read_to_string(&settings_path).unwrap();

        assert_eq!(first, second, "setup should be idempotent");

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_remove_gemini_preserves_hooks_disabled_and_user_hooks() {
        let (_dir, _test_home, settings_path, _guard) = gemini_test_env();

        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        let settings = serde_json::json!({
            "tools": {"enableHooks": true},
            "model": {"skipNextSpeakerCheck": false},
            "hooks": {
                "disabled": ["keep-me"],
                "SessionStart": [{
                    "matcher": "startup",
                    "hooks": [{
                        "name": "hcom-sessionstart",
                        "type": "command",
                        "command": "hcom gemini-sessionstart",
                        "timeout": 5000,
                    }],
                }],
                "BeforeAgent": [{
                    "matcher": "*",
                    "hooks": [
                        {
                            "name": "hcom-beforeagent",
                            "type": "command",
                            "command": "hcom gemini-beforeagent",
                            "timeout": 5000,
                        },
                        {
                            "name": "keep-other",
                            "type": "command",
                            "command": "echo hi",
                            "timeout": 1,
                        },
                    ],
                }],
            },
        });
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&settings).unwrap(),
        )
        .unwrap();

        assert!(remove_hooks_from_path(&settings_path));

        let updated = read_json(&settings_path);
        // disabled list preserved
        assert_eq!(updated["hooks"]["disabled"], serde_json::json!(["keep-me"]));
        // model preserved
        assert_eq!(updated["model"]["skipNextSpeakerCheck"], false);
        // SessionStart removed (only had hcom hooks)
        assert!(updated["hooks"].get("SessionStart").is_none());
        // BeforeAgent user hook preserved
        let before_agent = updated["hooks"]["BeforeAgent"].as_array().unwrap();
        assert_eq!(before_agent.len(), 1);
        let remaining = before_agent[0]["hooks"].as_array().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0]["name"], "keep-other");

        // Independent check: no hcom hooks remain
        let violations = independently_verify_no_hcom_hooks(&updated);
        assert!(violations.is_empty(), "hcom hooks remain: {violations:?}");

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_verify_gemini_detects_missing_hooks_enabled() {
        let (_dir, _test_home, settings_path, _guard) = gemini_test_env();

        assert!(setup_gemini_hooks(false));
        assert!(verify_gemini_hooks_installed(false));

        // Remove hooksConfig.enabled → verify should fail
        let mut settings = read_json(&settings_path);
        settings["hooksConfig"]
            .as_object_mut()
            .unwrap()
            .remove("enabled");
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&settings).unwrap(),
        )
        .unwrap();
        assert!(!verify_hooks_at(&settings_path, false));

        // Set hooksConfig.enabled to false → verify should fail
        settings["hooksConfig"]["enabled"] = Value::Bool(false);
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&settings).unwrap(),
        )
        .unwrap();
        assert!(!verify_hooks_at(&settings_path, false));

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_verify_accepts_alternate_hcom_prefix() {
        let (_dir, _test_home, settings_path, _guard) = gemini_test_env();

        assert!(setup_gemini_hooks(false));

        let mut settings = read_json(&settings_path);
        let (hook_type, _, cmd_suffix, _, _) = GEMINI_HOOK_CONFIGS[0];
        settings["hooks"][hook_type][0]["hooks"][0]["command"] =
            Value::String(format!("uvx hcom {cmd_suffix}"));
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&settings).unwrap(),
        )
        .unwrap();

        assert!(verify_hooks_at(&settings_path, false));

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_remove_gemini_cleans_legacy_enabled() {
        let (_dir, _test_home, settings_path, _guard) = gemini_test_env();

        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        let settings = serde_json::json!({
            "tools": {"enableHooks": true},
            "hooks": {
                "enabled": true,
                "SessionStart": [{
                    "matcher": "*",
                    "hooks": [{
                        "name": "hcom-sessionstart",
                        "type": "command",
                        "command": "hcom gemini-sessionstart",
                        "timeout": 5000,
                    }],
                }],
            },
        });
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&settings).unwrap(),
        )
        .unwrap();

        assert!(remove_hooks_from_path(&settings_path));

        let updated = read_json(&settings_path);
        // Legacy hooks.enabled should be gone
        assert!(
            updated.get("hooks").is_none() || updated["hooks"].get("enabled").is_none(),
            "legacy hooks.enabled should be removed"
        );
        // hcom hooks should be gone
        assert!(
            updated.get("hooks").is_none() || updated["hooks"].get("SessionStart").is_none(),
            "hcom hooks should be removed"
        );
    }

    #[test]
    #[serial]
    fn test_setup_gemini_remove_roundtrip() {
        let (_dir, _test_home, settings_path, _guard) = gemini_test_env();

        // Pre-populate with user data
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        let user_settings = serde_json::json!({
            "model": {"name": "gemini-2.5-pro"},
            "ui": {"theme": "Dark"},
        });
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&user_settings).unwrap(),
        )
        .unwrap();

        // Setup
        assert!(setup_gemini_hooks(false));
        let after_setup = read_json(&settings_path);
        let expected: Vec<(&str, &str)> = GEMINI_HOOK_CONFIGS
            .iter()
            .map(|&(ht, _, cmd, _, _)| (ht, cmd))
            .collect();
        let missing = independently_verify_hcom_hooks_present(&after_setup, &expected);
        assert!(
            missing.is_empty(),
            "after setup, missing hooks: {missing:?}"
        );

        // Remove
        assert!(remove_hooks_from_path(&settings_path));
        let after_remove = read_json(&settings_path);
        let violations = independently_verify_no_hcom_hooks(&after_remove);
        assert!(
            violations.is_empty(),
            "after remove, hcom hooks still present: {violations:?}"
        );

        // User data preserved
        assert_eq!(after_remove["model"]["name"], "gemini-2.5-pro");
        assert_eq!(after_remove["ui"]["theme"], "Dark");

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_gemini_handles_empty_file() {
        let (_dir, _test_home, settings_path, _guard) = gemini_test_env();

        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        std::fs::write(&settings_path, "{}").unwrap();

        assert!(setup_gemini_hooks(false));

        let settings = read_json(&settings_path);
        let expected: Vec<(&str, &str)> = GEMINI_HOOK_CONFIGS
            .iter()
            .map(|&(ht, _, cmd, _, _)| (ht, cmd))
            .collect();
        let missing = independently_verify_hcom_hooks_present(&settings, &expected);
        assert!(missing.is_empty(), "missing hooks: {missing:?}");

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_gemini_handles_no_file() {
        let (_dir, _test_home, settings_path, _guard) = gemini_test_env();

        assert!(!settings_path.exists());
        assert!(setup_gemini_hooks(false));
        assert!(settings_path.exists());

        let settings = read_json(&settings_path);
        let expected: Vec<(&str, &str)> = GEMINI_HOOK_CONFIGS
            .iter()
            .map(|&(ht, _, cmd, _, _)| (ht, cmd))
            .collect();
        let missing = independently_verify_hcom_hooks_present(&settings, &expected);
        assert!(missing.is_empty(), "missing hooks: {missing:?}");

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_gemini_mixed_hcom_and_user_hooks() {
        let (_dir, _test_home, settings_path, _guard) = gemini_test_env();

        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        let settings = serde_json::json!({
            "hooks": {
                "enabled": true,
                "SessionStart": [{
                    "matcher": "*",
                    "hooks": [
                        {"name": "hcom-sessionstart", "type": "command",
                         "command": "hcom gemini-sessionstart", "timeout": 5000},
                        {"name": "my-logger", "type": "command",
                         "command": "echo session started", "timeout": 1000},
                    ]
                }]
            }
        });
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&settings).unwrap(),
        )
        .unwrap();

        assert!(remove_hooks_from_path(&settings_path));

        let updated = read_json(&settings_path);
        // User hook should remain
        let session_hooks = updated["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(session_hooks.len(), 1);
        let hooks_list = session_hooks[0]["hooks"].as_array().unwrap();
        assert_eq!(hooks_list.len(), 1);
        assert_eq!(hooks_list[0]["name"], "my-logger");

        // No hcom hooks
        let violations = independently_verify_no_hcom_hooks(&updated);
        assert!(violations.is_empty());

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_gemini_handles_malformed_hooks() {
        let corrupt_cases: Vec<Value> = vec![
            Value::Null,
            Value::String("string".into()),
            serde_json::json!([]),
            serde_json::json!({"SessionStart": "not_a_list"}),
            serde_json::json!({"SessionStart": [null, "string", 123]}),
            serde_json::json!({"SessionStart": [{"matcher": "*", "hooks": "not_a_list"}]}),
        ];

        for corrupt_hooks in corrupt_cases {
            let (_dir, _test_home, settings_path, _guard) = gemini_test_env();
            std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();

            let settings = serde_json::json!({
                "hooks": corrupt_hooks,
                "ui": {"theme": "Dark"},
            });
            std::fs::write(
                &settings_path,
                serde_json::to_string_pretty(&settings).unwrap(),
            )
            .unwrap();

            // Should not crash
            let _ = setup_gemini_hooks(false);

            // User data should still be readable
            let updated = read_json(&settings_path);
            assert_eq!(updated["ui"]["theme"], "Dark");
        }
    }

    #[test]
    #[serial]
    fn test_setup_gemini_cleans_legacy_tools_allowed() {
        let (_dir, test_home, settings_path, _guard) = gemini_test_env();

        // Pre-populate with legacy hcom tools.allowed entries
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        let user_settings = serde_json::json!({
            "tools": {
                "allowed": [
                    "run_shell_command(hcom send)",
                    "run_shell_command(hcom list)",
                    "run_shell_command(git status)",
                ]
            }
        });
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&user_settings).unwrap(),
        )
        .unwrap();

        assert!(setup_gemini_hooks(true));

        let updated = read_json(&settings_path);
        let allowed = updated["tools"]["allowed"].as_array().unwrap();
        // User's non-hcom entry preserved
        assert!(
            allowed
                .iter()
                .any(|v| v.as_str() == Some("run_shell_command(git status)")),
            "user's git status entry should be preserved"
        );
        // Legacy hcom entries removed
        assert!(
            !allowed
                .iter()
                .any(|v| v.as_str().map(|s| s.contains("hcom")).unwrap_or(false)),
            "legacy hcom tools.allowed entries should be removed"
        );
        // Policy file should exist instead
        let policy_file = test_home.join(".gemini").join("policies").join("hcom.toml");
        assert!(policy_file.exists(), "policy file should be created");

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_setup_gemini_creates_and_removes_policy() {
        let (_dir, test_home, _settings_path, _guard) = gemini_test_env();

        assert!(setup_gemini_hooks(true));
        let policy_file = test_home.join(".gemini").join("policies").join("hcom.toml");
        assert!(policy_file.exists(), "policy file should be created");

        // Verify content
        let content = std::fs::read_to_string(&policy_file).unwrap();
        assert!(content.contains("[[rule]]"));
        assert!(content.contains("toolName = \"run_shell_command\""));
        assert!(content.contains("commandPrefix"));
        assert!(content.contains("decision = \"allow\""));
        assert!(content.contains("priority = 300"));

        // Setup without permissions should remove policy
        assert!(setup_gemini_hooks(false));
        assert!(
            !policy_file.exists(),
            "policy file should be removed when permissions disabled"
        );

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_setup_gemini_policy_idempotent() {
        let (_dir, test_home, _settings_path, _guard) = gemini_test_env();

        assert!(setup_gemini_hooks(true));
        let policy_file = test_home.join(".gemini").join("policies").join("hcom.toml");
        let first = std::fs::read_to_string(&policy_file).unwrap();

        assert!(setup_gemini_hooks(true));
        let second = std::fs::read_to_string(&policy_file).unwrap();

        assert_eq!(first, second, "policy content should be idempotent");

        drop(_guard);
    }

    #[test]
    #[serial]
    fn test_remove_gemini_hooks_removes_policy() {
        let (_dir, test_home, _settings_path, _guard) = gemini_test_env();

        assert!(setup_gemini_hooks(true));
        let policy_file = test_home.join(".gemini").join("policies").join("hcom.toml");
        assert!(policy_file.exists());

        remove_gemini_hooks();
        assert!(
            !policy_file.exists(),
            "policy file should be removed on hook removal"
        );

        drop(_guard);
    }
}
