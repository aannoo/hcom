//! Kimi Code CLI hook handlers and config.toml management.
//!
//! Kimi hooks are declared in `~/.kimi-code/config.toml` under `[[hooks]]` array
//! tables. Each hook receives JSON on stdin and uses exit code / stdout for
//! results (0 = allow, 2 = block).

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{Value, json};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table};

use crate::db::{HcomDb, InstanceRow};
use crate::hooks::{HookPayload, HookResult, common};
use crate::instance_binding;
use crate::instance_lifecycle as lifecycle;
use crate::instances;
use crate::log;
use crate::paths;
use crate::shared::context::HcomContext;
use crate::shared::{ST_ACTIVE, ST_LISTENING};

const HOOK_TIMEOUT_SECS: i64 = 30;
const KIMI_HOOK_COMMANDS: &[(&str, &str)] = &[
    ("SessionStart", "kimi-sessionstart"),
    ("UserPromptSubmit", "kimi-userpromptsubmit"),
    ("PreToolUse", "kimi-pretooluse"),
    ("PostToolUse", "kimi-posttooluse"),
    ("Stop", "kimi-stop"),
    ("SessionEnd", "kimi-sessionend"),
    ("SubagentStart", "kimi-subagentstart"),
    ("SubagentStop", "kimi-subagentstop"),
    ("Notification", "kimi-notification"),
];

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("existing Kimi config at {} could not be read: {source}", path.display())]
    ExistingReadFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("existing Kimi config at {} is not valid TOML: {source}", path.display())]
    ExistingParseFailed {
        path: PathBuf,
        #[source]
        source: toml_edit::TomlError,
    },
    #[error("failed to create Kimi config directory {}: {source}", path.display())]
    DirCreateFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("atomic write to {} failed: {source}", path.display())]
    AtomicWriteFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("post-write Kimi hook verification failed for {}", .0.display())]
    PostWriteVerifyFailed(PathBuf),
}

// ── Config path helpers ─────────────────────────────────────────────────

fn kimi_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("KIMI_CONFIG_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    dirs::home_dir().unwrap_or_default().join(".kimi-code")
}

pub fn get_kimi_settings_path() -> PathBuf {
    kimi_config_dir().join("config.toml")
}

fn build_kimi_hook_command(command: &str) -> String {
    let mut parts = crate::runtime_env::get_hcom_prefix();
    parts.push(command.to_string());
    parts.join(" ")
}

fn is_hcom_kimi_command(command: &str) -> bool {
    let prefix = build_kimi_hook_command("");
    command.starts_with(&prefix)
        && KIMI_HOOK_COMMANDS
            .iter()
            .any(|(_, suffix)| command.trim() == format!("{}{}", prefix.trim_end(), suffix))
}

// ── TOML manipulation ───────────────────────────────────────────────────

fn read_toml_document(path: &Path) -> Result<DocumentMut, SetupError> {
    if !path.exists() {
        return Ok(DocumentMut::new());
    }
    let content =
        std::fs::read_to_string(path).map_err(|source| SetupError::ExistingReadFailed {
            path: path.to_path_buf(),
            source,
        })?;
    content.parse::<DocumentMut>().map_err(|source| SetupError::ExistingParseFailed {
        path: path.to_path_buf(),
        source,
    })
}

fn write_toml(path: &Path, doc: &DocumentMut) -> Result<(), SetupError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| SetupError::DirCreateFailed {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let content = doc.to_string();
    paths::atomic_write_io(path, &content).map_err(|source| SetupError::AtomicWriteFailed {
        path: path.to_path_buf(),
        source,
    })
}

fn merge_hcom_hooks(doc: &mut DocumentMut) {
    let hooks_item = doc
        .entry("hooks")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));

    if let Item::ArrayOfTables(arr) = hooks_item {
        let mut filtered = ArrayOfTables::new();
        for i in 0..arr.len() {
            if let Some(table) = arr.get(i) {
                let keep = table
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map(|cmd| !is_hcom_kimi_command(cmd))
                    .unwrap_or(true);
                if keep {
                    filtered.push(table.clone());
                }
            }
        }
        *arr = filtered;

        for (event, command_suffix) in KIMI_HOOK_COMMANDS {
            let mut table = Table::new();
            table.insert("event", toml_edit::value(*event));
            table.insert("command", toml_edit::value(build_kimi_hook_command(command_suffix)));
            table.insert("timeout", toml_edit::value(HOOK_TIMEOUT_SECS));
            arr.push(table);
        }
    }
}

fn remove_hcom_hooks(doc: &mut DocumentMut) {
    let Some(hooks_item) = doc.get_mut("hooks") else {
        return;
    };
    let Item::ArrayOfTables(arr) = hooks_item else {
        return;
    };
    let mut filtered = ArrayOfTables::new();
    for i in 0..arr.len() {
        if let Some(table) = arr.get(i) {
            let keep = table
                .get("command")
                .and_then(|v| v.as_str())
                .map(|cmd| !is_hcom_kimi_command(cmd))
                .unwrap_or(true);
            if keep {
                filtered.push(table.clone());
            }
        }
    }
    *arr = filtered;
    if arr.is_empty() {
        doc.remove("hooks");
    }
}

fn verify_hooks_at(path: &Path) -> bool {
    let Ok(doc) = read_toml_document(path) else {
        return false;
    };
    let Some(Item::ArrayOfTables(arr)) = doc.get("hooks") else {
        return false;
    };
    KIMI_HOOK_COMMANDS.iter().all(|(event, command_suffix)| {
        let expected_cmd = build_kimi_hook_command(command_suffix);
        (0..arr.len()).any(|i| {
            arr.get(i).is_some_and(|table| {
                table.get("event").and_then(|v| v.as_str()) == Some(*event)
                    && table.get("command").and_then(|v| v.as_str()) == Some(&expected_cmd)
                    && table.get("timeout").and_then(|v| v.as_integer()).is_some()
            })
        })
    })
}

// ── Public setup / verify / remove ──────────────────────────────────────

pub fn remove_kimi_hooks() -> bool {
    let path = get_kimi_settings_path();
    if !path.exists() {
        return true;
    }
    match read_toml_document(&path) {
        Ok(mut doc) => {
            remove_hcom_hooks(&mut doc);
            write_toml(&path, &doc).is_ok()
        }
        Err(_) => false,
    }
}

pub fn try_setup_kimi_hooks(_include_permissions: bool) -> Result<(), SetupError> {
    let path = get_kimi_settings_path();
    let mut doc = read_toml_document(&path)?;
    merge_hcom_hooks(&mut doc);
    write_toml(&path, &doc)?;
    if !verify_hooks_at(&path) {
        return Err(SetupError::PostWriteVerifyFailed(path));
    }
    Ok(())
}

pub fn verify_kimi_hooks_installed(_check_permissions: bool) -> bool {
    verify_hooks_at(&get_kimi_settings_path())
}

// ── Instance helpers ────────────────────────────────────────────────────

fn resolve_instance(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
) -> Option<InstanceRow> {
    instance_binding::resolve_instance_from_binding(
        db,
        payload.session_id.as_deref(),
        ctx.process_id.as_deref(),
    )
}

pub fn derive_kimi_transcript_path(session_id: &str) -> Option<String> {
    let base = dirs::home_dir()?.join(".kimi").join("sessions");
    if !base.exists() {
        return None;
    }
    let Ok(entries) = std::fs::read_dir(&base) else {
        return None;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let candidate = path.join(session_id).join("context.jsonl");
        if candidate.exists() {
            return Some(candidate.to_string_lossy().to_string());
        }
    }
    None
}

fn update_position(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload, instance_name: &str) {
    let mut updates = serde_json::Map::new();
    if let Some(session_id) = payload.session_id.as_ref().filter(|s| !s.is_empty()) {
        updates.insert("session_id".into(), Value::String(session_id.clone()));
        if let Some(tp) = derive_kimi_transcript_path(session_id) {
            updates.insert("transcript_path".into(), Value::String(tp));
        }
    }
    let cwd = payload
        .raw
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or_else(|| ctx.cwd.to_str().unwrap_or(""));
    if !cwd.is_empty() {
        updates.insert("directory".into(), Value::String(cwd.to_string()));
    }
    if !updates.is_empty() {
        instances::update_instance_position(db, instance_name, &updates);
    }
}

// ── Hook handlers ───────────────────────────────────────────────────────

fn handle_sessionstart(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    if ctx.process_id.is_none() {
        return HookResult::Allow {
            additional_context: Some(format!(
                "[hcom available - run '{} start' to participate]",
                crate::runtime_env::build_hcom_command()
            )),
            system_message: None,
            delivery_ack: None,
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
        "kimi.sessionstart.bind",
        &format!(
            "instance={:?} session_id={} process_id={:?}",
            instance_name, session_id, ctx.process_id,
        ),
    );

    let instance_name = match instance_name {
        Some(name) => name,
        None => {
            if let Some(ref pid) = ctx.process_id {
                match instance_binding::create_orphaned_pty_identity(
                    db,
                    session_id,
                    Some(pid.as_str()),
                    "kimi",
                ) {
                    Some(name) => name,
                    None => return hook_noop(),
                }
            } else {
                return hook_noop();
            }
        }
    };

    let _ = db.rebind_instance_session(&instance_name, session_id);
    instance_binding::capture_and_store_launch_context(db, &instance_name);
    update_position(db, ctx, payload, &instance_name);

    lifecycle::set_status(
        db,
        &instance_name,
        ST_LISTENING,
        "start",
        Default::default(),
    );

    crate::runtime_env::set_terminal_title(&instance_name);
    crate::relay::worker::ensure_worker(true);

    if let Ok(Some(inst)) = db.get_instance_full(&instance_name)
        && let Some(bootstrap) =
            common::inject_bootstrap_once(db, ctx, &instance_name, &inst, &inst.tool)
    {
        return HookResult::Allow {
            additional_context: Some(bootstrap),
            system_message: None,
            delivery_ack: None,
        };
    }

    hook_noop()
}

fn handle_userpromptsubmit(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
) -> HookResult {
    let instance = match resolve_instance(db, ctx, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };
    let instance_name = &instance.name;
    update_position(db, ctx, payload, instance_name);

    if let Some(prepared) = common::prepare_pending_messages(db, instance_name) {
        return HookResult::Allow {
            additional_context: Some(prepared.formatted),
            system_message: None,
            delivery_ack: Some(prepared.ack),
        };
    }

    hook_noop()
}

fn handle_pretooluse(db: &HcomDb, _ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_instance(db, _ctx, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };
    let instance_name = &instance.name;

    let detail = crate::hooks::family::extract_tool_detail(
        "kimi",
        &payload.tool_name,
        &payload.tool_input,
    );
    if !detail.is_empty() {
        lifecycle::set_status(
            db,
            instance_name,
            ST_ACTIVE,
            &detail,
            Default::default(),
        );
    }

    hook_noop()
}

fn handle_posttooluse(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_instance(db, ctx, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };
    let instance_name = &instance.name;

    if let Some(prepared) = common::prepare_pending_messages(db, instance_name) {
        return HookResult::Allow {
            additional_context: Some(prepared.formatted),
            system_message: None,
            delivery_ack: Some(prepared.ack),
        };
    }

    hook_noop()
}

fn handle_stop(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_instance(db, ctx, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };
    let instance_name = &instance.name;

    if let Some(prepared) = common::prepare_pending_messages(db, instance_name) {
        return HookResult::Block {
            reason: prepared.formatted,
        };
    }

    hook_noop()
}

fn handle_sessionend(db: &HcomDb, _ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_instance(db, _ctx, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };
    let instance_name = &instance.name;

    common::finalize_session(
        db,
        instance_name,
        "sessionend",
        None,
    );

    hook_noop()
}

fn handle_subagentstart(
    _db: &HcomDb,
    _ctx: &HcomContext,
    _payload: &HookPayload,
) -> HookResult {
    hook_noop()
}

fn handle_subagentstop(
    _db: &HcomDb,
    _ctx: &HcomContext,
    _payload: &HookPayload,
) -> HookResult {
    hook_noop()
}

fn handle_notification(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
) -> HookResult {
    let instance = match resolve_instance(db, ctx, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };
    let instance_name = &instance.name;

    if let Some(prepared) = common::prepare_pending_messages(db, instance_name) {
        return HookResult::Allow {
            additional_context: Some(prepared.formatted),
            system_message: None,
            delivery_ack: Some(prepared.ack),
        };
    }

    hook_noop()
}

fn hook_noop() -> HookResult {
    HookResult::Allow {
        additional_context: None,
        system_message: None,
        delivery_ack: None,
    }
}

fn get_handler(
    hook_name: &str,
) -> Option<fn(&HcomDb, &HcomContext, &HookPayload) -> HookResult> {
    match hook_name {
        "kimi-sessionstart" => Some(handle_sessionstart),
        "kimi-userpromptsubmit" => Some(handle_userpromptsubmit),
        "kimi-pretooluse" => Some(handle_pretooluse),
        "kimi-posttooluse" => Some(handle_posttooluse),
        "kimi-stop" => Some(handle_stop),
        "kimi-sessionend" => Some(handle_sessionend),
        "kimi-subagentstart" => Some(handle_subagentstart),
        "kimi-subagentstop" => Some(handle_subagentstop),
        "kimi-notification" => Some(handle_notification),
        _ => None,
    }
}

// ── Dispatch ────────────────────────────────────────────────────────────

pub fn dispatch_kimi_hook(hook_name: &str) -> i32 {
    let start = Instant::now();

    let ctx = HcomContext::from_os();

    let mut input = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut input) {
        log::log_error(
            "hooks",
            "kimi.stdin_error",
            &format!("hook={} err={}", hook_name, e),
        );
        return 0;
    }

    let raw: Value = match serde_json::from_slice(&input) {
        Ok(v) => v,
        Err(e) => {
            log::log_error(
                "hooks",
                "kimi.parse_error",
                &format!("hook={} err={}", hook_name, e),
            );
            return 0;
        }
    };

    let payload = HookPayload::from_kimi(hook_name, raw);

    // Pre-gate: skip UserPromptSubmit for non-participants
    if !ctx.is_launched && hook_name == "kimi-userpromptsubmit" {
        let sid = match payload.session_id.as_deref() {
            Some(sid) => sid,
            None => return 0,
        };
        if let Ok(db) = HcomDb::open() {
            if db.get_session_binding(sid).ok().flatten().is_none() {
                return 0;
            }
        } else {
            return 0;
        }
    }

    if !crate::paths::ensure_hcom_directories() {
        return 0;
    }

    let db = match HcomDb::open() {
        Ok(db) => db,
        Err(e) => {
            log::log_error("hooks", "kimi.db.error", &format!("{}", e));
            return 0;
        }
    };

    if !common::hook_gate_check(&ctx, &db) {
        return 0;
    }

    let handler = match get_handler(hook_name) {
        Some(h) => h,
        None => {
            log::log_error(
                "hooks",
                "kimi.dispatch.unknown",
                &format!("Unknown Kimi hook: {}", hook_name),
            );
            return 0;
        }
    };

    let result = common::dispatch_with_panic_guard(
        "kimi",
        hook_name,
        HookResult::Allow {
            additional_context: None,
            system_message: None,
            delivery_ack: None,
        },
        || handler(&db, &ctx, &payload),
    );

    let exit_code = match &result {
        HookResult::Allow { .. } => 0,
        HookResult::Block { .. } => 2,
        HookResult::UpdateInput { .. } => 0,
    };

    match result {
        HookResult::Allow {
            additional_context: Some(ctx),
            ..
        } => {
            let output = json!({
                "hookSpecificOutput": {
                    "message": ctx,
                }
            });
            println!("{}", output);
        }
        HookResult::Block { reason } => {
            let output = json!({
                "hookSpecificOutput": {
                    "permissionDecision": "deny",
                    "permissionDecisionReason": reason,
                }
            });
            eprintln!("{}", reason);
            println!("{}", output);
        }
        _ => {}
    }

    let total_ms = start.elapsed().as_secs_f64() * 1000.0;
    log::log_info(
        "hooks",
        "kimi.dispatch.timing",
        &format!(
            "hook={} exit_code={} total_ms={:.2}",
            hook_name, exit_code, total_ms
        ),
    );

    exit_code
}
