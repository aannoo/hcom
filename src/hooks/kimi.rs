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
use crate::shared::{ST_ACTIVE, ST_BLOCKED, ST_LISTENING};

const HOOK_TIMEOUT_SECS: i64 = 30;
const KIMI_HOOK_COMMANDS: &[(&str, &str)] = &[
    ("SessionStart", "kimi-sessionstart"),
    ("UserPromptSubmit", "kimi-userpromptsubmit"),
    ("PreToolUse", "kimi-pretooluse"),
    ("PostToolUse", "kimi-posttooluse"),
    ("PermissionRequest", "kimi-permissionrequest"),
    ("PermissionResult", "kimi-permissionresult"),
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

/// Kimi's data root: config.toml, sessions, credentials all live here.
/// Overridden via `KIMI_CODE_HOME` (kimi does not honor any other dir variable),
/// defaulting to `~/.kimi-code`.
fn kimi_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("KIMI_CODE_HOME")
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
    // Compare against the canonical command string for each hook suffix.
    // (An earlier `format!("{}{}", prefix.trim_end(), suffix)` dropped the space
    // between prefix and suffix, so this never matched — causing every re-setup
    // to duplicate all hcom hooks instead of replacing them.)
    let trimmed = command.trim();
    KIMI_HOOK_COMMANDS
        .iter()
        .any(|(_, suffix)| trimmed == build_kimi_hook_command(suffix))
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
    content
        .parse::<DocumentMut>()
        .map_err(|source| SetupError::ExistingParseFailed {
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
            table.insert(
                "command",
                toml_edit::value(build_kimi_hook_command(command_suffix)),
            );
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

// ── Permission allowlist (auto-approve hcom's own commands) ──────────────
//
// Kimi gates every tool call behind a permission check. Its `[[permission.rules]]`
// are matched top-to-bottom, first match wins (config-files.md#permission). To let
// a launched agent run its own `hcom` commands unattended — without `--yolo`, which
// would also auto-approve unrelated tools — hcom prepends `decision = "allow"`
// rules for its safe self-commands ahead of any user rules.

/// Allow-rule patterns hcom installs (current hcom command prefix).
fn kimi_permission_patterns() -> Vec<String> {
    let prefix = crate::runtime_env::build_hcom_command();
    common::SAFE_HCOM_COMMANDS
        .iter()
        .map(|command| format!("Bash({prefix} {command}*)"))
        .collect()
}

/// All patterns hcom may have written (both `hcom` and `uvx hcom` prefixes), so
/// removal/re-merge can recognize and strip stale managed rules.
fn all_kimi_permission_patterns() -> Vec<String> {
    let mut patterns = Vec::new();
    for prefix in ["hcom", "uvx hcom"] {
        for command in common::SAFE_HCOM_COMMANDS {
            patterns.push(format!("Bash({prefix} {command}*)"));
        }
    }
    patterns
}

fn is_hcom_permission_pattern(pattern: &str) -> bool {
    all_kimi_permission_patterns()
        .iter()
        .any(|managed| managed == pattern)
}

/// Get a `&mut ArrayOfTables` for `[[permission.rules]]`, creating the parent
/// `[permission]` table on demand. Returns `None` if `permission`/`rules` exist
/// but are not tables of the expected shape (leave a user's odd config alone).
fn permission_rules_mut(doc: &mut DocumentMut) -> Option<&mut ArrayOfTables> {
    let permission = doc
        .entry("permission")
        .or_insert_with(|| Item::Table(Table::new()));
    let Item::Table(permission) = permission else {
        return None;
    };
    let rules = permission
        .entry("rules")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    match rules {
        Item::ArrayOfTables(arr) => Some(arr),
        _ => None,
    }
}

fn merge_hcom_permissions(doc: &mut DocumentMut) {
    let Some(arr) = permission_rules_mut(doc) else {
        return;
    };

    // Rebuild with hcom allow-rules first (first-match-wins ordering), then the
    // user's existing non-hcom rules. This makes the merge idempotent and keeps
    // hcom's allows ahead of any broad user `ask`/`deny` on `Bash`.
    let mut rebuilt = ArrayOfTables::new();
    for pattern in kimi_permission_patterns() {
        let mut table = Table::new();
        table.insert("decision", toml_edit::value("allow"));
        table.insert("pattern", toml_edit::value(pattern));
        table.insert("reason", toml_edit::value("hcom auto-approve"));
        rebuilt.push(table);
    }
    for i in 0..arr.len() {
        if let Some(table) = arr.get(i) {
            let is_managed = table
                .get("pattern")
                .and_then(|v| v.as_str())
                .map(is_hcom_permission_pattern)
                .unwrap_or(false);
            if !is_managed {
                rebuilt.push(table.clone());
            }
        }
    }
    *arr = rebuilt;
}

fn remove_hcom_permissions(doc: &mut DocumentMut) {
    let Some(Item::Table(permission)) = doc.get_mut("permission") else {
        return;
    };
    if let Some(Item::ArrayOfTables(arr)) = permission.get_mut("rules") {
        let mut filtered = ArrayOfTables::new();
        for i in 0..arr.len() {
            if let Some(table) = arr.get(i) {
                let is_managed = table
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .map(is_hcom_permission_pattern)
                    .unwrap_or(false);
                if !is_managed {
                    filtered.push(table.clone());
                }
            }
        }
        *arr = filtered;
        if arr.is_empty() {
            permission.remove("rules");
        }
    }
    if permission.is_empty() {
        doc.remove("permission");
    }
}

fn verify_permissions_at(path: &Path) -> bool {
    let Ok(doc) = read_toml_document(path) else {
        return false;
    };
    let Some(Item::Table(permission)) = doc.get("permission") else {
        return false;
    };
    let Some(Item::ArrayOfTables(arr)) = permission.get("rules") else {
        return false;
    };
    let present: Vec<&str> = (0..arr.len())
        .filter_map(|i| arr.get(i))
        .filter(|t| t.get("decision").and_then(|v| v.as_str()) == Some("allow"))
        .filter_map(|t| t.get("pattern").and_then(|v| v.as_str()))
        .collect();
    kimi_permission_patterns()
        .iter()
        .all(|expected| present.iter().any(|p| p == expected))
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
            remove_hcom_permissions(&mut doc);
            write_toml(&path, &doc).is_ok()
        }
        Err(_) => false,
    }
}

pub fn try_setup_kimi_hooks(include_permissions: bool) -> Result<(), SetupError> {
    let path = get_kimi_settings_path();
    let mut doc = read_toml_document(&path)?;
    merge_hcom_hooks(&mut doc);
    if include_permissions {
        merge_hcom_permissions(&mut doc);
    } else {
        remove_hcom_permissions(&mut doc);
    }
    write_toml(&path, &doc)?;
    if !verify_hooks_at(&path) {
        return Err(SetupError::PostWriteVerifyFailed(path.clone()));
    }
    if include_permissions && !verify_permissions_at(&path) {
        return Err(SetupError::PostWriteVerifyFailed(path));
    }
    Ok(())
}

pub fn verify_kimi_hooks_installed(check_permissions: bool) -> bool {
    let path = get_kimi_settings_path();
    verify_hooks_at(&path) && (!check_permissions || verify_permissions_at(&path))
}

// ── Instance helpers ────────────────────────────────────────────────────

fn resolve_instance(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> Option<InstanceRow> {
    instance_binding::resolve_instance_from_binding(
        db,
        payload.session_id.as_deref(),
        ctx.process_id.as_deref(),
    )
}

/// Resolve a Kimi session's transcript file.
///
/// Kimi stores each session's wire log at:
/// ```text
///   $KIMI_CODE_HOME/sessions/wd_<dir>_<hash>/<session_id>/agents/main/wire.jsonl
/// ```
/// The working-directory bucket (`wd_*`) is unknown here, so scan the buckets
/// for the one containing this session. `session_id` already carries the
/// `session_` prefix (matching the on-disk directory name).
pub fn derive_kimi_transcript_path(session_id: &str) -> Option<String> {
    let base = kimi_config_dir().join("sessions");
    if !base.exists() {
        return None;
    }
    let entries = std::fs::read_dir(&base).ok()?;
    for entry in entries.flatten() {
        let wd = entry.path();
        if !wd.is_dir() {
            continue;
        }
        let candidate = wd
            .join(session_id)
            .join("agents")
            .join("main")
            .join("wire.jsonl");
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

    // NOTE: bootstrap is intentionally NOT injected here. Kimi does not add
    // SessionStart hook output to model context (it is an observation-only
    // event), so bootstrapping happens on the first UserPromptSubmit instead
    // (see handle_userpromptsubmit).
    hook_noop()
}

fn handle_userpromptsubmit(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_instance(db, ctx, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };
    let instance_name = &instance.name;
    update_position(db, ctx, payload, instance_name);

    // Bootstrap is delivered here, NOT at SessionStart: kimi only injects
    // UserPromptSubmit hook output into model context — SessionStart output is
    // not added to context (see kimi hooks docs). Prepend it to the first
    // delivery so a launched agent learns it's on hcom.
    //
    // KNOWN LIMITATION (kimi 0.9.0): this makes the bootstrap *visible* — kimi
    // wraps UserPromptSubmit output as a `<hook_result>` block in the turn,
    // unlike codex/claude which inject it invisibly into the system prompt.
    // Kimi has no per-instance invisible channel today: no `--system-prompt`
    // flag, no `-c` config override, no `systemPrompt` config key, and AGENTS.md
    // (its only invisible system-prompt source) loads from shared paths so it
    // can't carry a per-instance name without polluting the workspace. Revisit
    // and switch to an invisible launch-time injection (mirroring codex's
    // developer_instructions path) if a future kimi release adds a system-prompt
    // append flag or env var.
    let bootstrap =
        common::inject_bootstrap_once(db, ctx, instance_name, &instance, &instance.tool);
    let pending = common::prepare_pending_messages(db, instance_name);

    let additional_context = match (&bootstrap, &pending) {
        (Some(boot), Some(p)) => Some(format!("{boot}\n\n{}", p.formatted)),
        (Some(boot), None) => Some(boot.clone()),
        (None, Some(p)) => Some(p.formatted.clone()),
        (None, None) => None,
    };

    if let Some(additional_context) = additional_context {
        return HookResult::Allow {
            additional_context: Some(additional_context),
            system_message: None,
            delivery_ack: pending.map(|p| p.ack),
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

    let detail =
        crate::hooks::family::extract_tool_detail("kimi", &payload.tool_name, &payload.tool_input);
    if !detail.is_empty() {
        lifecycle::set_status(db, instance_name, ST_ACTIVE, &detail, Default::default());
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

/// PermissionRequest (observation-only): kimi fires this just before it blocks
/// waiting for the user to approve/reject a tool call. Mark the agent `blocked`
/// so `hcom list` reflects the stall and the delivery gate (require_idle) holds
/// off injecting until the user responds. `PermissionResult` clears it.
///
/// hcom's own `[[permission.rules]]` allow-rules mean an agent's `hcom send`
/// etc. are auto-approved and never reach an `ask` — so this only fires for
/// tool calls that genuinely need a human (mirrors claude's handle_permission_request).
fn handle_permissionrequest(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_instance(db, ctx, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };
    let instance_name = &instance.name;

    let detail =
        crate::hooks::family::extract_tool_detail("kimi", &payload.tool_name, &payload.tool_input);
    lifecycle::set_status(
        db,
        instance_name,
        ST_BLOCKED,
        "approval",
        lifecycle::StatusUpdate {
            detail: &detail,
            ..Default::default()
        },
    );

    hook_noop()
}

/// PermissionResult (observation-only): kimi fires this once approval resolves.
/// In every case the agent is still mid-turn — an approved tool now runs, a
/// declined one feeds rejection back to the model — so flip out of `blocked`
/// back to `active`. The `Stop` hook sets `listening` when the turn actually
/// ends. (PermissionResult carries no tool_input, so the tool name is the best
/// available detail; mirrors claude's PostToolUse "approved:" restore.)
///
/// The payload's `decision` (`approved`/`rejected`/`cancelled`/`error`) drives a
/// decision-aware context so a declined call isn't mislabeled as approved:
/// `approved:<tool>` only when actually approved, `denied:<tool>` otherwise.
fn handle_permissionresult(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_instance(db, ctx, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };
    let instance_name = &instance.name;

    let decision = payload.raw.get("decision").and_then(Value::as_str);
    let verb = if decision == Some("approved") {
        "approved"
    } else {
        "denied"
    };
    lifecycle::set_status(
        db,
        instance_name,
        ST_ACTIVE,
        &format!("{verb}:{}", payload.tool_name),
        Default::default(),
    );

    hook_noop()
}

fn handle_stop(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_instance(db, ctx, payload) {
        Some(inst) => inst,
        None => return hook_noop(),
    };
    let instance_name = &instance.name;

    if let Some(prepared) = common::prepare_pending_messages(db, instance_name) {
        // The Stop hook delivers via Block{reason}, which cannot carry the ack
        // back to the dispatch — commit inline so the cursor advances.
        common::commit_delivery_ack(db, &prepared.ack);
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

    common::finalize_session(db, instance_name, "sessionend", None);

    hook_noop()
}

fn handle_subagentstart(_db: &HcomDb, _ctx: &HcomContext, _payload: &HookPayload) -> HookResult {
    hook_noop()
}

fn handle_subagentstop(_db: &HcomDb, _ctx: &HcomContext, _payload: &HookPayload) -> HookResult {
    hook_noop()
}

fn handle_notification(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
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

fn get_handler(hook_name: &str) -> Option<fn(&HcomDb, &HcomContext, &HookPayload) -> HookResult> {
    match hook_name {
        "kimi-sessionstart" => Some(handle_sessionstart),
        "kimi-userpromptsubmit" => Some(handle_userpromptsubmit),
        "kimi-pretooluse" => Some(handle_pretooluse),
        "kimi-posttooluse" => Some(handle_posttooluse),
        "kimi-permissionrequest" => Some(handle_permissionrequest),
        "kimi-permissionresult" => Some(handle_permissionresult),
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
            delivery_ack,
            ..
        } => {
            let output = json!({
                "hookSpecificOutput": {
                    "message": ctx,
                }
            });
            println!("{}", output);
            // Advance the delivery cursor only after the message is handed to
            // kimi (stdout). Without this the PTY delivery loop never observes
            // the cursor advancing and keeps re-injecting `<hcom>`.
            if let Some(ack) = delivery_ack {
                common::commit_delivery_ack(&db, &ack);
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(doc: &DocumentMut) -> &ArrayOfTables {
        match doc.get("permission").and_then(|p| p.get("rules")) {
            Some(Item::ArrayOfTables(arr)) => arr,
            _ => panic!("expected [[permission.rules]]"),
        }
    }

    #[test]
    fn is_hcom_kimi_command_matches_canonical_commands() {
        // Each installed hook command must be recognized as hcom-managed, else
        // re-setup duplicates them instead of replacing them.
        for (_, suffix) in KIMI_HOOK_COMMANDS {
            let cmd = build_kimi_hook_command(suffix);
            assert!(
                is_hcom_kimi_command(&cmd),
                "should recognize installed hcom hook command: {cmd}"
            );
        }
        assert!(!is_hcom_kimi_command("echo hello"));
        assert!(!is_hcom_kimi_command("hcom send @x -- hi"));
    }

    #[test]
    fn permission_events_are_registered_and_dispatchable() {
        // Both observation-only permission events must be (a) installed into
        // config.toml via KIMI_HOOK_COMMANDS and (b) routable to a handler,
        // else the blocked-on-approval status transition never fires.
        for (event, suffix) in [
            ("PermissionRequest", "kimi-permissionrequest"),
            ("PermissionResult", "kimi-permissionresult"),
        ] {
            assert!(
                KIMI_HOOK_COMMANDS.contains(&(event, suffix)),
                "{event}/{suffix} must be in KIMI_HOOK_COMMANDS"
            );
            assert!(
                get_handler(suffix).is_some(),
                "{suffix} must resolve to a handler"
            );
        }
        // The spec's routing list (Tool::from_hook_name) must agree, or the
        // installed hook command would never reach dispatch_kimi_hook.
        let spec_names = crate::tool::Tool::Kimi.hooks();
        assert!(spec_names.contains(&"kimi-permissionrequest"));
        assert!(spec_names.contains(&"kimi-permissionresult"));
    }

    #[test]
    fn merge_hooks_is_idempotent() {
        let mut doc = DocumentMut::new();
        merge_hcom_hooks(&mut doc);
        let first = match doc.get("hooks") {
            Some(Item::ArrayOfTables(arr)) => arr.len(),
            _ => panic!("expected [[hooks]]"),
        };
        merge_hcom_hooks(&mut doc);
        let second = match doc.get("hooks") {
            Some(Item::ArrayOfTables(arr)) => arr.len(),
            _ => panic!("expected [[hooks]]"),
        };
        assert_eq!(first, KIMI_HOOK_COMMANDS.len());
        assert_eq!(
            first, second,
            "re-merging hooks must not duplicate existing hcom hooks"
        );
    }

    #[test]
    fn merge_prepends_allow_rules_for_all_safe_commands() {
        let mut doc = DocumentMut::new();
        merge_hcom_permissions(&mut doc);

        let arr = rules(&doc);
        let expected = kimi_permission_patterns();
        // Our allow-rules come first, one per safe command, all decision=allow.
        assert!(arr.len() >= expected.len());
        for (i, pat) in expected.iter().enumerate() {
            let table = arr.get(i).expect("rule present");
            assert_eq!(
                table.get("decision").and_then(|v| v.as_str()),
                Some("allow")
            );
            assert_eq!(
                table.get("pattern").and_then(|v| v.as_str()),
                Some(pat.as_str())
            );
        }
        assert!(verify_permissions_at_doc(&doc));
    }

    #[test]
    fn merge_is_idempotent_and_keeps_user_rules_after_ours() {
        let mut doc: DocumentMut = r#"
[[permission.rules]]
decision = "ask"
pattern = "Bash"
"#
        .parse()
        .unwrap();

        merge_hcom_permissions(&mut doc);
        let after_first = rules(&doc).len();
        merge_hcom_permissions(&mut doc);
        let after_second = rules(&doc).len();
        assert_eq!(
            after_first, after_second,
            "re-merging must not duplicate managed rules"
        );

        // The user's broad `ask Bash` rule survives and sits AFTER our allows,
        // so first-match-wins still auto-approves hcom commands.
        let arr = rules(&doc);
        let last = arr.get(arr.len() - 1).unwrap();
        assert_eq!(last.get("decision").and_then(|v| v.as_str()), Some("ask"));
        assert_eq!(last.get("pattern").and_then(|v| v.as_str()), Some("Bash"));
        assert!(verify_permissions_at_doc(&doc));
    }

    #[test]
    fn remove_strips_only_managed_rules() {
        let mut doc: DocumentMut = r#"
[[permission.rules]]
decision = "deny"
pattern = "Bash(rm -rf*)"
"#
        .parse()
        .unwrap();

        merge_hcom_permissions(&mut doc);
        remove_hcom_permissions(&mut doc);

        // The user's deny rule remains; managed allows are gone.
        let arr = rules(&doc);
        assert_eq!(arr.len(), 1);
        let only = arr.get(0).unwrap();
        assert_eq!(only.get("decision").and_then(|v| v.as_str()), Some("deny"));
        assert!(!verify_permissions_at_doc(&doc));
    }

    #[test]
    fn remove_drops_empty_permission_table() {
        let mut doc = DocumentMut::new();
        merge_hcom_permissions(&mut doc);
        remove_hcom_permissions(&mut doc);
        assert!(
            doc.get("permission").is_none(),
            "permission table should be removed when no rules remain"
        );
    }

    // Mirror of verify_permissions_at but against an in-memory document so the
    // tests never touch the real ~/.kimi-code/config.toml.
    fn verify_permissions_at_doc(doc: &DocumentMut) -> bool {
        let Some(Item::Table(permission)) = doc.get("permission") else {
            return false;
        };
        let Some(Item::ArrayOfTables(arr)) = permission.get("rules") else {
            return false;
        };
        let present: Vec<&str> = (0..arr.len())
            .filter_map(|i| arr.get(i))
            .filter(|t| t.get("decision").and_then(|v| v.as_str()) == Some("allow"))
            .filter_map(|t| t.get("pattern").and_then(|v| v.as_str()))
            .collect();
        kimi_permission_patterns()
            .iter()
            .all(|expected| present.iter().any(|p| p == expected))
    }
}
