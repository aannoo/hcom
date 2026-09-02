//! Codex native hook handlers and settings management.

use std::collections::{HashMap, HashSet};
use std::io::Write;
#[cfg(not(test))]
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
#[cfg(not(test))]
use std::process::Stdio;
use std::sync::OnceLock;
#[cfg(not(test))]
use std::sync::mpsc;
#[cfg(not(test))]
use std::sync::{Arc, Mutex};
#[cfg(not(test))]
use std::time::Duration;
use std::time::UNIX_EPOCH;

use serde_json::Value;
use toml_edit::{DocumentMut, Item, value};

use crate::db::{HcomDb, InstanceRow};
use crate::hooks::{HookPayload, HookResult, common, family};
use crate::instance_binding;
use crate::instance_lifecycle as lifecycle;
use crate::instances;
use crate::log;
use crate::paths;
use crate::shared::context::HcomContext;
use crate::shared::{ST_ACTIVE, ST_LISTENING};

use super::common::SAFE_HCOM_COMMANDS;

const HCOM_TRIGGER: &str = "<hcom>";
const CODEX_DIRECTORY_PIN_KEY: &str = "codex_directory_pin_v1";
const CODEX_DIRECTORY_PIN_SOURCE: &str = "explicit-start-relocate-v1";

pub(crate) fn directory_override_launch_context(directory: &str, session_id: &str) -> String {
    serde_json::json!({
        CODEX_DIRECTORY_PIN_KEY: {
            "directory": directory,
            "session_id": session_id,
            "source": CODEX_DIRECTORY_PIN_SOURCE,
        }
    })
    .to_string()
}

fn pinned_directory_from_value(value: &Value, session_id: &str) -> Option<String> {
    let pin = value.as_object()?;
    let directory = pin.get("directory")?.as_str()?.trim();
    let pinned_session = pin.get("session_id")?.as_str()?.trim();
    let source = pin.get("source")?.as_str()?;
    if directory.is_empty()
        || !Path::new(directory).is_absolute()
        || session_id.is_empty()
        || pinned_session != session_id
        || source != CODEX_DIRECTORY_PIN_SOURCE
    {
        return None;
    }
    Some(directory.to_string())
}

fn pinned_directory_is_live(directory: &str) -> bool {
    std::fs::symlink_metadata(directory)
        .ok()
        .is_some_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
}

fn launch_context_has_directory_pin(launch_context: Option<&str>) -> bool {
    launch_context
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .is_some_and(|context| context.get(CODEX_DIRECTORY_PIN_KEY).is_some())
}

pub(crate) fn pinned_directory_from_launch_context(
    launch_context: Option<&str>,
    session_id: &str,
) -> Option<String> {
    let context: Value = serde_json::from_str(launch_context?).ok()?;
    pinned_directory_from_value(context.get(CODEX_DIRECTORY_PIN_KEY)?, session_id)
}

fn pinned_directory_from_snapshot(snapshot: &Value, session_id: &str) -> Option<String> {
    pinned_directory_from_value(snapshot.get(CODEX_DIRECTORY_PIN_KEY)?, session_id)
}

pub(crate) fn directory_override_snapshot_value(
    launch_context: Option<&str>,
    session_id: Option<&str>,
) -> Value {
    let Some(session_id) = session_id.filter(|value| !value.is_empty()) else {
        return Value::Null;
    };
    let Some(context) = launch_context.and_then(|raw| serde_json::from_str::<Value>(raw).ok())
    else {
        return Value::Null;
    };
    let Some(pin) = context.get(CODEX_DIRECTORY_PIN_KEY) else {
        return Value::Null;
    };
    if pinned_directory_from_value(pin, session_id).is_none() {
        return Value::Null;
    }
    pin.clone()
}

const CODEX_HOOK_COMMANDS: &[(&str, &str, Option<&str>)] = &[
    (
        "SessionStart",
        "codex-sessionstart",
        Some("startup|resume|clear"),
    ),
    ("UserPromptSubmit", "codex-userpromptsubmit", None),
    ("PreToolUse", "codex-pretooluse", Some("Bash")),
    ("PostToolUse", "codex-posttooluse", Some("Bash")),
    ("Stop", "codex-stop", None),
];
const HCOM_TOOL_NAMES: &[&str] = &[
    "claude",
    "gemini",
    "codex",
    "opencode",
    "antigravity",
    "agy",
];
const CODEX_HOOKS_FEATURE_RENAME_VERSION: (u64, u64, u64) = (0, 129, 0);
const CODEX_HOOK_TRUST_MIN_VERSION: (u64, u64, u64) = (0, 131, 0);
/// Wire value of Codex's `HookSource::User` variant.
///
/// `codex_protocol::protocol::HookSource` is `rename_all = "snake_case"`
/// (codex-rs/protocol/src/protocol.rs:1528) and the app-server v2 mirror is
/// `rename_all = "camelCase"` (the `v2_enum_from_core!` macro in
/// codex-rs/app-server-protocol/src/protocol/v2/shared.rs:21-48, applied at
/// v2/hook.rs:42). Both encodings render the single-word `User` variant as
/// "user", so one literal covers the whole protocol surface.
const CODEX_HOOK_SOURCE_USER: &str = "user";
/// Trust statuses Codex already permits without the bypass flag. Everything
/// else (`untrusted`, `modified`, or a status hcom does not recognize) is what
/// `--dangerously-bypass-hook-trust` would newly unlock — see the gate at
/// codex-rs/hooks/src/engine/discovery.rs:565-571.
const CODEX_ALREADY_PERMITTED_TRUST_STATUSES: &[&str] = &["trusted", "managed"];
/// Every Codex hook event that can carry declarations in a hooks.json file or a
/// `[hooks]` TOML table (codex-rs/config/src/hook_config.rs:36-59). Wider than
/// `CODEX_HOOK_COMMANDS`, which lists only the events hcom itself installs.
const CODEX_ALL_HOOK_EVENTS: &[&str] = &[
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PreCompact",
    "PostCompact",
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "SubagentStart",
    "SubagentStop",
    "Stop",
];
/// Subdirectories of `$CODEX_HOME/plugins` that hold installed plugins
/// (codex-rs/core-plugins/src/store.rs:21-22).
const CODEX_PLUGIN_STORE_DIRS: &[&str] = &["cache", "data"];
/// Codex's default `project_root_markers`
/// (codex-rs/config/src/project_root_markers.rs:5).
const CODEX_DEFAULT_PROJECT_ROOT_MARKERS: &[&str] = &[".git"];
const HCOM_CODEX_CLI_VERSION_KEY: &str = "hcom_codex_cli_version";
const HCOM_HOOK_DEFINITION_HASH_KEY: &str = "hcom_hook_definition_hash";
#[cfg(not(test))]
const CODEX_APP_SERVER_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(not(test))]
const CODEX_APP_SERVER_STDERR_LIMIT: usize = 8192;
type CodexHookHandler = fn(&HcomDb, &HcomContext, &HookPayload) -> HookResult;

#[derive(Clone, Debug, Eq, PartialEq)]
struct CodexHookTrustEntry {
    key: String,
    command: String,
    current_hash: String,
}

/// One hook from a `codex app-server hooks/list` response, reduced to the
/// fields hcom needs to tell its own hooks apart from everyone else's and to
/// predict what `--dangerously-bypass-hook-trust` would unlock.
///
/// Field names on the wire are camelCase (`HookMetadata` in
/// codex-rs/app-server-protocol/src/protocol/v2/plugin.rs:513-542); the
/// snake_case spellings of the core protocol are accepted too.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CodexHookListEntry {
    key: Option<String>,
    command: Option<String>,
    source: Option<String>,
    source_path: Option<PathBuf>,
    enabled: bool,
    trust_status: Option<String>,
    current_hash: Option<String>,
}

/// hcom's launch-time verdict on Codex's hook-trust gate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CodexHookTrustState {
    /// Nothing to do: Codex predates the trust gate, or hcom's own trust state
    /// in `hooks.state` is exact and its hooks will run on their own.
    Trusted,
    /// Codex's own `hooks/list` inventory says the invocation-wide bypass would
    /// unlock nothing except hcom's hooks.
    BypassSafeFromHooksList,
    /// `hooks/list` was unavailable, but a purely local scan of every hook
    /// definition that could be in scope found only hcom's own.
    BypassSafeFromLocalScan,
    /// The bypass would — or might — unlock a hook hcom does not own.
    BypassUnsafe { reason: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CodexHookLocalEntry {
    key: String,
    command: String,
    definition_hash: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodexHooksFeatureKey {
    CodexHooks,
    Hooks,
}

impl CodexHooksFeatureKey {
    fn as_str(self) -> &'static str {
        match self {
            Self::CodexHooks => "codex_hooks",
            Self::Hooks => "hooks",
        }
    }

    fn alternate(self) -> &'static str {
        match self {
            Self::CodexHooks => "hooks",
            Self::Hooks => "codex_hooks",
        }
    }
}

fn hook_noop() -> HookResult {
    HookResult::Allow {
        additional_context: None,
        system_message: None,
        delivery_ack: None,
    }
}

fn codex_event_name(hook_name: &str) -> &'static str {
    CODEX_HOOK_COMMANDS
        .iter()
        .find(|(_, cmd, _)| *cmd == hook_name)
        .map(|(event, _, _)| *event)
        .unwrap_or("Unknown")
}

/// Derive Codex transcript path from session_id.
pub fn derive_codex_transcript_path(session_id: &str) -> Option<String> {
    if session_id.is_empty() {
        return None;
    }

    let codex_base = std::env::var("CODEX_HOME").ok().unwrap_or_else(|| {
        dirs::home_dir()
            .map(|h| h.join(".codex").to_string_lossy().to_string())
            .unwrap_or_default()
    });

    let sessions_dir = PathBuf::from(&codex_base).join("sessions");
    let pattern = format!(
        "{}/**/rollout-*-{}.jsonl",
        sessions_dir.display(),
        session_id
    );

    match glob::glob(&pattern) {
        Ok(entries) => {
            let mut matches: Vec<PathBuf> = entries.filter_map(|e| e.ok()).collect();
            if matches.is_empty() {
                return None;
            }
            matches.sort_by(|a, b| {
                let ta = a
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH);
                let tb = b
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH);
                tb.cmp(&ta)
            });
            matches.first().map(|p| p.to_string_lossy().to_string())
        }
        Err(_) => None,
    }
}

/// Normalize Windows verbatim paths before storing them in the instance row.
///
/// Codex can report the same transcript as `C:\...` on the initial hook and
/// `\\?\C:\...` after a resume. Keep the database representation stable so
/// resume and transcript lookup continue to refer to the same file.
fn normalize_codex_transcript_path(path: &str) -> String {
    const VERBATIM_PREFIX: &str = "\\\\?\\";
    const VERBATIM_UNC_PREFIX: &str = "\\\\?\\UNC\\";

    if let Some(unc_path) = path.strip_prefix(VERBATIM_UNC_PREFIX) {
        format!("\\\\{unc_path}")
    } else {
        path.strip_prefix(VERBATIM_PREFIX)
            .unwrap_or(path)
            .to_string()
    }
}

fn resolve_instance_codex(db: &HcomDb, ctx: &HcomContext, session_id: &str) -> Option<InstanceRow> {
    instance_binding::resolve_instance_from_binding(
        db,
        Some(session_id).filter(|s| !s.is_empty()),
        ctx.process_id.as_deref(),
    )
}

fn bind_vanilla_instance_codex(
    db: &HcomDb,
    session_id: &str,
    transcript_path: Option<&str>,
) -> Option<String> {
    let pending = common::get_pending_instances(db);
    if pending.is_empty() {
        return None;
    }

    let derived_path = if transcript_path.is_none() || transcript_path == Some("") {
        derive_codex_transcript_path(session_id)
    } else {
        None
    };
    let effective_path = transcript_path
        .filter(|s| !s.is_empty())
        .or(derived_path.as_deref())?;
    let effective_path = normalize_codex_transcript_path(effective_path);

    let instance_name = common::find_last_bind_marker(&effective_path)?;

    family::bind_vanilla_instance(
        db,
        &instance_name,
        Some(session_id).filter(|s| !s.is_empty()),
        Some(&effective_path),
        "codex",
        "codex-sessionstart",
    )
}

fn resolve_codex_instance(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
) -> Option<InstanceRow> {
    let session_id = payload.session_id.as_deref().unwrap_or("");
    if let Some(instance) = resolve_instance_codex(db, ctx, session_id) {
        return Some(instance);
    }

    let bound_name =
        bind_vanilla_instance_codex(db, session_id, payload.transcript_path.as_deref())?;
    db.get_instance_full(&bound_name).ok().flatten()
}

fn update_codex_position(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
    instance_name: &str,
) {
    let mut updates = serde_json::Map::new();
    let session_id = payload.session_id.as_deref().unwrap_or("");
    let launch_context = db
        .get_instance_full(instance_name)
        .ok()
        .flatten()
        .and_then(|row| row.launch_context);
    let pin_present = launch_context_has_directory_pin(launch_context.as_deref());
    let pinned_directory =
        pinned_directory_from_launch_context(launch_context.as_deref(), session_id);
    let cwd = match pinned_directory {
        Some(pinned) if pinned_directory_is_live(&pinned) => Some(pinned),
        Some(_) => {
            log::log_warn(
                "hooks",
                "codex.directory_pin_invalid",
                &format!("instance={instance_name} session={session_id} directory_mutated=false"),
            );
            None
        }
        None if pin_present => {
            log::log_warn(
                "hooks",
                "codex.directory_pin_invalid",
                &format!("instance={instance_name} session={session_id} directory_mutated=false"),
            );
            None
        }
        None => Some(
            payload
                .raw
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| ctx.cwd.to_string_lossy().to_string()),
        ),
    };
    if let Some(cwd) = cwd.filter(|cwd| !cwd.is_empty()) {
        updates.insert("directory".into(), Value::String(cwd));
    }
    if let Some(session_id) = payload.session_id.as_ref().filter(|s| !s.is_empty()) {
        updates.insert("session_id".into(), Value::String(session_id.clone()));
    }
    let transcript_path = payload.transcript_path.clone().or_else(|| {
        payload
            .session_id
            .as_deref()
            .and_then(derive_codex_transcript_path)
    });
    if let Some(tp) = transcript_path {
        updates.insert(
            "transcript_path".into(),
            Value::String(normalize_codex_transcript_path(&tp)),
        );
    }
    if !updates.is_empty() {
        instances::update_instance_position(db, instance_name, &updates);
    }
}

/// Prepare pending messages for a Codex instance.
///
/// Only additionalContext — no systemMessage. Codex TUI renders both
/// as separate visible lines ("warning:" + "hook context:"), causing
/// double output for every delivered message.
fn prepare_codex_delivery(db: &HcomDb, instance_name: &str) -> Option<HookResult> {
    common::prepare_pending_messages(db, instance_name).map(|prepared| HookResult::Allow {
        additional_context: Some(prepared.formatted),
        system_message: None,
        delivery_ack: Some(prepared.ack),
    })
}

fn resolve_and_update_codex_instance(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
) -> Option<InstanceRow> {
    let instance = resolve_codex_instance(db, ctx, payload)?;
    update_codex_position(db, ctx, payload, &instance.name);
    Some(instance)
}

fn set_prompt_active(db: &HcomDb, instance_name: &str) {
    lifecycle::set_status(db, instance_name, ST_ACTIVE, "prompt", Default::default());
}

fn codex_hook_cwd(ctx: &HcomContext, payload: &HookPayload) -> String {
    payload
        .raw
        .get("cwd")
        .and_then(|value| value.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| ctx.cwd.to_string_lossy().to_string())
}

/// Accept a native Codex lifecycle payload only when the task identity exposed
/// to the hook process agrees with the payload. The payload session id is the
/// single canonical identity used by SessionStart after this check; ambient
/// CLI environment alone never authorizes a registration move.
fn canonical_sessionstart_id<'a>(ctx: &HcomContext, payload: &'a HookPayload) -> Option<&'a str> {
    let payload_id = payload.session_id.as_deref()?.trim();
    let context_id = ctx.codex_thread_id.as_deref()?.trim();
    let session_env = ctx
        .raw_env
        .get("CODEX_SESSION_ID")
        .map(String::as_str)
        .map(str::trim)
        .unwrap_or("");
    if payload_id.is_empty()
        || context_id.is_empty()
        || session_env.is_empty()
        || payload_id != context_id
        || payload_id != session_env
    {
        log::log_warn(
            "hooks",
            "codex.sessionstart_identity_mismatch",
            &format!(
                "payload_session_id={} context_thread_id={} context_session_id={} mutated=false",
                payload_id, context_id, session_env
            ),
        );
        return None;
    }
    Some(payload_id)
}

fn codex_restore_paths_match(stopped: &str, current: &str) -> bool {
    let stopped = std::fs::canonicalize(stopped).unwrap_or_else(|_| PathBuf::from(stopped));
    let current = std::fs::canonicalize(current).unwrap_or_else(|_| PathBuf::from(current));

    #[cfg(windows)]
    {
        stopped
            .to_string_lossy()
            .eq_ignore_ascii_case(&current.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        stopped == current
    }
}

/// Recreate a Codex row that stop/exit cleanup deleted at native SessionStart.
///
/// `bind_session_to_process` can recover the canonical name from the durable
/// stopped event without a process binding, but its generic reconstruction
/// path needs a launch placeholder. Codex Desktop has no such placeholder, so
/// SessionStart restores the row from the same stopped snapshot after checking
/// that the tool and exact stopped session still describe this task. An
/// explicit, session-scoped directory pin can preserve a deliberate logical
/// project relocation; unpinned tasks keep the same-directory guard.
fn restore_missing_codex_instance(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
    session_id: &str,
    instance_name: &str,
) -> bool {
    if let Some(instance) = db.get_instance_full(instance_name).ok().flatten() {
        let current_cwd = codex_hook_cwd(ctx, payload);
        let pin_present = launch_context_has_directory_pin(instance.launch_context.as_deref());
        let pinned_directory =
            pinned_directory_from_launch_context(instance.launch_context.as_deref(), session_id);
        let directory_matches = match pinned_directory {
            Some(ref pinned) if pinned_directory_is_live(pinned) => {
                codex_restore_paths_match(&instance.directory, pinned)
            }
            Some(_) => false,
            None if pin_present => false,
            None => {
                instance.directory.is_empty()
                    || (!current_cwd.is_empty()
                        && codex_restore_paths_match(&instance.directory, &current_cwd))
            }
        };
        return instance.tool == "codex"
            && !instances::is_remote_instance(&instance)
            && instance.session_id.as_deref() == Some(session_id)
            && directory_matches;
    }

    let data: String = match db.conn().query_row(
        "SELECT data FROM events
         WHERE type = 'life'
           AND instance = ?1
           AND json_extract(data, '$.action') = 'stopped'
         ORDER BY id DESC LIMIT 1",
        rusqlite::params![instance_name],
        |row| row.get(0),
    ) {
        Ok(data) => data,
        Err(_) => return false,
    };
    let data: Value = match serde_json::from_str(&data) {
        Ok(data) => data,
        Err(_) => return false,
    };
    let Some(snapshot) = data.get("snapshot") else {
        return false;
    };

    if snapshot.get("tool").and_then(Value::as_str) != Some("codex")
        || snapshot.get("session_id").and_then(Value::as_str) != Some(session_id)
        || snapshot
            .get("origin_device_id")
            .and_then(Value::as_str)
            .is_some_and(|device| !device.is_empty())
    {
        return false;
    }

    let current_cwd = codex_hook_cwd(ctx, payload);
    let stopped_cwd = snapshot
        .get("directory")
        .and_then(Value::as_str)
        .unwrap_or("");
    let pin_present = snapshot.get(CODEX_DIRECTORY_PIN_KEY).is_some();
    let pinned_directory = pinned_directory_from_snapshot(snapshot, session_id);
    let directory = match pinned_directory.as_deref() {
        Some(pinned)
            if pinned_directory_is_live(pinned)
                && !stopped_cwd.is_empty()
                && codex_restore_paths_match(stopped_cwd, pinned) =>
        {
            pinned
        }
        Some(_) => return false,
        None if pin_present => return false,
        None if !stopped_cwd.is_empty()
            && !current_cwd.is_empty()
            && codex_restore_paths_match(stopped_cwd, &current_cwd) =>
        {
            current_cwd.as_str()
        }
        None => return false,
    };

    let transcript_path = payload
        .transcript_path
        .as_deref()
        .filter(|path| !path.is_empty())
        .or_else(|| {
            snapshot
                .get("transcript_path")
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty())
        });
    if !instance_binding::initialize_instance_in_position_file(
        db,
        instance_name,
        Some(session_id),
        snapshot.get("parent_session_id").and_then(Value::as_str),
        snapshot.get("parent_name").and_then(Value::as_str),
        snapshot.get("agent_id").and_then(Value::as_str),
        transcript_path,
        Some("codex"),
        snapshot
            .get("background")
            .and_then(Value::as_i64)
            .is_some_and(|background| background != 0),
        snapshot.get("tag").and_then(Value::as_str),
        snapshot.get("wait_timeout").and_then(Value::as_i64),
        snapshot.get("subagent_timeout").and_then(Value::as_i64),
        snapshot.get("hints").and_then(Value::as_str),
        Some(directory),
    ) {
        return false;
    }

    if let Some(pinned) = pinned_directory {
        let context = directory_override_launch_context(&pinned, session_id);
        if db.store_launch_context(instance_name, &context).is_err() {
            let _ = db.delete_instance(instance_name);
            return false;
        }
    }

    let mut updates = serde_json::Map::new();
    if let Some(last_event_id) = snapshot.get("last_event_id").and_then(Value::as_i64) {
        updates.insert("last_event_id".into(), Value::from(last_event_id));
    }
    if let Some(name_announced) = snapshot.get("name_announced").and_then(Value::as_i64) {
        updates.insert("name_announced".into(), Value::from(name_announced));
    }
    let _ = db.update_instance_fields(instance_name, &updates);

    db.get_instance_full(instance_name).ok().flatten().is_some()
}

fn resolve_sessionstart_instance(
    db: &HcomDb,
    ctx: &HcomContext,
    payload: &HookPayload,
    session_id: &str,
) -> Option<String> {
    // Codex Desktop has no HCOM_PROCESS_ID. SessionStart is the one safe point
    // to restore an identity removed by stop/exit cleanup: doing this in the
    // shared resolver would undo an intentional stop on every later hook.
    if let Some(process_id) = ctx.process_id.as_deref() {
        return instance_binding::bind_session_to_process(db, session_id, Some(process_id));
    }

    // A live session binding is already foreign-keyed to an existing row. Check
    // ownership before accepting it, but do not send Desktop through the generic
    // stopped-row path: without a launch placeholder that path attempts a rebind
    // before the row exists and emits the very FK error this recovery prevents.
    if let Ok(Some(instance_name)) = db.get_session_binding(session_id) {
        return restore_missing_codex_instance(db, ctx, payload, session_id, &instance_name)
            .then_some(instance_name);
    }

    if let Ok(Some(instance_name)) = db.find_stopped_instance_by_session_id(session_id)
        && restore_missing_codex_instance(db, ctx, payload, session_id, &instance_name)
    {
        return Some(instance_name);
    }

    resolve_codex_instance(db, ctx, payload).map(|instance| instance.name)
}

fn handle_sessionstart(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let Some(session_id) = canonical_sessionstart_id(ctx, payload) else {
        return hook_noop();
    };

    let instance_name = match resolve_sessionstart_instance(db, ctx, payload, session_id) {
        Some(name) => name,
        None => return hook_noop(),
    };

    let _ = db.rebind_instance_session(&instance_name, session_id);
    instance_binding::capture_and_store_launch_context(db, &instance_name);
    update_codex_position(db, ctx, payload, &instance_name);
    lifecycle::set_status(
        db,
        &instance_name,
        ST_LISTENING,
        "start",
        Default::default(),
    );
    crate::runtime_env::set_terminal_title(&instance_name);
    crate::relay::worker::ensure_worker(true);
    common::notify_hook_instance_with_db(db, &instance_name);

    // Bootstrap is injected at launch time via developer_instructions flag,
    // not here — Codex TUI renders hook output visibly ("hook context:").
    hook_noop()
}

fn handle_userpromptsubmit(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_and_update_codex_instance(db, ctx, payload) {
        Some(instance) => instance,
        None => return hook_noop(),
    };

    let prompt = payload
        .raw
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if prompt.trim() != HCOM_TRIGGER {
        set_prompt_active(db, &instance.name);
        return hook_noop();
    }

    if let Some(result) = prepare_codex_delivery(db, &instance.name) {
        result
    } else {
        set_prompt_active(db, &instance.name);
        hook_noop()
    }
}

fn handle_pretooluse(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_and_update_codex_instance(db, ctx, payload) {
        Some(instance) => instance,
        None => return hook_noop(),
    };

    common::update_tool_status(
        db,
        &instance.name,
        "codex",
        &payload.tool_name,
        &payload.tool_input,
    );
    hook_noop()
}

fn handle_posttooluse(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_and_update_codex_instance(db, ctx, payload) {
        Some(instance) => instance,
        None => return hook_noop(),
    };

    prepare_codex_delivery(db, &instance.name).unwrap_or_else(hook_noop)
}

fn handle_stop(db: &HcomDb, ctx: &HcomContext, payload: &HookPayload) -> HookResult {
    let instance = match resolve_and_update_codex_instance(db, ctx, payload) {
        Some(instance) => instance,
        None => return hook_noop(),
    };

    lifecycle::set_status(db, &instance.name, ST_LISTENING, "", Default::default());
    common::notify_hook_instance_with_db(db, &instance.name);
    hook_noop()
}

fn get_codex_handler(hook_name: &str) -> Option<CodexHookHandler> {
    match hook_name {
        "codex-sessionstart" => Some(handle_sessionstart),
        "codex-userpromptsubmit" => Some(handle_userpromptsubmit),
        "codex-pretooluse" => Some(handle_pretooluse),
        "codex-posttooluse" => Some(handle_posttooluse),
        "codex-stop" => Some(handle_stop),
        _ => None,
    }
}

fn dispatch_result_to_stdout(db: &HcomDb, hook_name: &str, result: HookResult) -> i32 {
    match result {
        HookResult::Allow {
            additional_context,
            system_message,
            delivery_ack,
        } => {
            let output = match (hook_name, additional_context, system_message) {
                ("codex-stop", None, None) => Some(serde_json::json!({})),
                (_, Some(ctx), sys) => {
                    let mut obj = serde_json::Map::new();
                    if let Some(msg) = sys {
                        obj.insert("systemMessage".into(), Value::String(msg));
                    }
                    obj.insert(
                        "hookSpecificOutput".into(),
                        serde_json::json!({
                            "hookEventName": codex_event_name(hook_name),
                            "additionalContext": ctx,
                        }),
                    );
                    Some(Value::Object(obj))
                }
                (_, None, Some(msg)) => Some(serde_json::json!({ "systemMessage": msg })),
                _ => None,
            };
            if let Some(json) = output {
                let mut stdout = std::io::stdout().lock();
                if serde_json::to_writer(&mut stdout, &json).is_ok()
                    && stdout.flush().is_ok()
                    && let Some(ack) = delivery_ack.as_ref()
                {
                    common::commit_delivery_ack(db, ack);
                }
            }
            0
        }
        HookResult::Block { reason } => {
            // Codex hooks on exit 2 read the reason from stderr, not stdout.
            let _ = std::io::stderr().lock().write_all(reason.as_bytes());
            2
        }
        HookResult::UpdateInput { updated_input } => {
            let _ = serde_json::to_writer(
                std::io::stdout().lock(),
                &serde_json::json!({ "updatedInput": updated_input }),
            );
            0
        }
    }
}

/// Main entry point for native Codex hooks.
pub fn dispatch_codex_hook_native(hook_name: &str) -> i32 {
    let start = std::time::Instant::now();
    let raw: Value = match serde_json::from_reader(std::io::stdin().lock()) {
        Ok(v) => v,
        Err(e) => {
            log::log_error(
                "hooks",
                "codex.parse_error",
                &format!("hook={hook_name} err={e}"),
            );
            return 0;
        }
    };

    let db = match HcomDb::open() {
        Ok(db) => db,
        Err(e) => {
            log::log_warn(
                "hooks",
                "codex.db_error",
                &format!("hook={hook_name} err={e}"),
            );
            return 0;
        }
    };

    let ctx = HcomContext::from_os();
    if !common::hook_gate_check(&ctx, &db) {
        return 0;
    }

    let payload = HookPayload::from_codex_native(codex_event_name(hook_name), raw);
    let result = common::dispatch_with_panic_guard("codex", hook_name, hook_noop(), || {
        get_codex_handler(hook_name)
            .map(|handler| handler(&db, &ctx, &payload))
            .unwrap_or_else(hook_noop)
    });

    let exit_code = dispatch_result_to_stdout(&db, hook_name, result);
    let total_ms = start.elapsed().as_secs_f64() * 1000.0;
    log::log_info(
        "hooks",
        "codex.dispatch.timing",
        &format!(
            "hook={} total_ms={:.2} exit_code={}",
            hook_name, total_ms, exit_code
        ),
    );
    exit_code
}

// ---------------------------------------------------------------------------
// Settings management — hooks.json, config.toml, execpolicy
// ---------------------------------------------------------------------------

/// Resolve the Codex config directory.
///
/// Priority: CODEX_HOME env var → tool_config_root()/.codex
fn codex_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CODEX_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    crate::runtime_env::tool_config_root().join(".codex")
}

/// Get path to Codex config.toml.
pub fn get_codex_config_path() -> PathBuf {
    codex_config_dir().join("config.toml")
}

/// Get path to Codex hooks.json.
pub fn get_codex_hooks_path() -> PathBuf {
    codex_config_dir().join("hooks.json")
}

/// Get path to Codex execpolicy rules directory.
pub fn get_codex_rules_path() -> PathBuf {
    codex_config_dir().join("rules")
}

/// Strip a Windows verbatim prefix and collapse `.`/`..` components.
///
/// Purely lexical, so it works on paths that do not exist.
fn lexically_normalized(path: &Path) -> PathBuf {
    use std::path::Component;

    let text = path.to_string_lossy();
    let plain = text
        .strip_prefix(r"\\?\UNC\")
        .map(|unc| format!(r"\\{unc}"))
        .or_else(|| text.strip_prefix(r"\\?\").map(str::to_string));
    let plain = plain.map(PathBuf::from);
    let path = plain.as_deref().unwrap_or(path);

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

/// Whether two paths name the same file, without requiring either to exist.
///
/// Codex passes hook source paths through `AbsolutePathBuf::from_absolute_path`
/// (codex-rs/utils/absolute-path/src/lib.rs:58), which absolutizes lexically but
/// does not resolve symlinks, so a `sourcePath` from Codex can differ from
/// hcom's own `get_codex_hooks_path()` by a `.`/`..` component, a verbatim
/// Windows prefix, or by one side having been canonicalized. Compare lexically
/// first and only then pay for canonicalization.
fn paths_equivalent(a: &Path, b: &Path) -> bool {
    if a == b || lexically_normalized(a) == lexically_normalized(b) {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Whether a `hooks.state` key names a handler inside hcom's own hooks.json.
///
/// Codex derives these keys as
/// `hook_key(&source.key_source, event_name, group_index, handler_index)` —
/// `"<key_source>:<event_label>:<group>:<handler>"`
/// (codex-rs/hooks/src/lib.rs:105-115) — and for a JSON hook source the
/// key_source is that file's path (codex-rs/hooks/src/engine/discovery.rs:148).
/// Splitting from the right keeps Windows drive colons inside the path part.
fn hook_state_key_belongs_to_hcom_hooks_json(key: &str, hooks_path: &Path) -> bool {
    let mut parts = key.rsplitn(4, ':');
    let (Some(handler_index), Some(group_index), Some(event_label), Some(key_source)) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    if handler_index.parse::<usize>().is_err() || group_index.parse::<usize>().is_err() {
        return false;
    }
    if !CODEX_HOOK_COMMANDS
        .iter()
        .any(|(event, _, _)| codex_hook_event_state_label(event) == event_label)
    {
        return false;
    }
    paths_equivalent(Path::new(key_source), hooks_path)
}

/// Build a quote-free Windows hook command.
///
/// Codex wraps the complete command in outer quotes before passing it to
/// `cmd.exe /C`; embedding quotes around the executable makes the resulting
/// token literal `\\"...\\"` and exits 1. `windows_current_hcom_executable`
/// has already converted spaced paths to a safe DOS short path and rejected
/// cmd metacharacters.
#[cfg(windows)]
fn build_pinned_windows_codex_hook_command(executable: &str, command: &str) -> String {
    debug_assert!(
        executable.chars().all(|ch| !ch.is_whitespace()
            && !matches!(ch, '"' | '&' | '|' | '<' | '>' | '^' | '%' | '!'))
    );
    format!("{executable} {command}")
}

fn build_codex_hook_command(command: &str) -> String {
    #[cfg(windows)]
    if let Some(executable) = crate::runtime_env::windows_current_hcom_executable() {
        return build_pinned_windows_codex_hook_command(&executable, command);
    }

    let mut parts = crate::runtime_env::get_hcom_prefix();
    parts.push(command.to_string());
    parts.join(" ")
}

fn build_expected_hook_json() -> Value {
    let mut hooks = serde_json::Map::new();
    for (event, command, matcher) in CODEX_HOOK_COMMANDS {
        let mut group = serde_json::Map::new();
        if let Some(matcher) = matcher {
            group.insert("matcher".into(), Value::String((*matcher).to_string()));
        }
        group.insert(
            "hooks".into(),
            Value::Array(vec![serde_json::json!({
                "type": "command",
                "command": build_codex_hook_command(command),
            })]),
        );
        hooks.insert(
            (*event).to_string(),
            Value::Array(vec![Value::Object(group)]),
        );
    }
    Value::Object(serde_json::Map::from_iter([(
        "hooks".into(),
        Value::Object(hooks),
    )]))
}

fn is_hcom_codex_command(command: &str) -> bool {
    CODEX_HOOK_COMMANDS.iter().any(|(_, suffix, _)| {
        command == build_codex_hook_command(suffix) || command.ends_with(suffix)
    })
}

fn is_hcom_legacy_notify(item: &Item) -> bool {
    match item {
        Item::Value(v) => {
            if let Some(s) = v.as_str() {
                return s.contains("hcom") && s.contains("codex-notify");
            }
            if let Some(arr) = v.as_array() {
                let values: Vec<&str> = arr.iter().filter_map(|entry| entry.as_str()).collect();
                return values.iter().any(|s| s.contains("hcom"))
                    && values.iter().any(|s| s.contains("codex-notify"));
            }
            false
        }
        _ => false,
    }
}

fn merge_hcom_hooks(existing: &mut Value) {
    if !existing.is_object() {
        *existing = serde_json::json!({ "hooks": {} });
    }

    // Strip existing hcom hooks first so stale matchers don't accumulate.
    remove_hcom_hooks_from_json(existing);

    let hooks_obj = existing
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    if !hooks_obj.is_object() {
        *hooks_obj = serde_json::json!({});
    }

    let current_hooks = hooks_obj.as_object_mut().unwrap();
    let expected = build_expected_hook_json();
    let expected_hooks = expected["hooks"].as_object().unwrap();

    for (event, expected_groups) in expected_hooks {
        let entry = current_hooks
            .entry(event.clone())
            .or_insert_with(|| Value::Array(Vec::new()));
        if !entry.is_array() {
            *entry = Value::Array(Vec::new());
        }
        let groups = entry.as_array_mut().unwrap();

        for expected_group in expected_groups.as_array().unwrap() {
            let expected_matcher = expected_group.get("matcher").and_then(|v| v.as_str());
            let new_hooks = expected_group["hooks"].as_array().unwrap();

            let matched = groups
                .iter_mut()
                .find(|g| g.get("matcher").and_then(|v| v.as_str()) == expected_matcher);

            if let Some(group) = matched {
                if !group.get("hooks").is_some_and(|v| v.is_array()) {
                    group
                        .as_object_mut()
                        .unwrap()
                        .insert("hooks".into(), Value::Array(Vec::new()));
                }
                let hooks_arr = group
                    .get_mut("hooks")
                    .and_then(|v| v.as_array_mut())
                    .unwrap();
                hooks_arr.retain(|h| {
                    !h.get("command")
                        .and_then(|v| v.as_str())
                        .is_some_and(is_hcom_codex_command)
                });
                hooks_arr.extend(new_hooks.iter().cloned());
            } else {
                groups.push(expected_group.clone());
            }
        }
    }
}

fn remove_hcom_hooks_from_json(existing: &mut Value) {
    let Some(hooks_obj) = existing.get_mut("hooks").and_then(|v| v.as_object_mut()) else {
        return;
    };

    for (_, groups) in hooks_obj.iter_mut() {
        let Some(groups_arr) = groups.as_array_mut() else {
            continue;
        };
        for group in groups_arr.iter_mut() {
            if let Some(hooks_arr) = group.get_mut("hooks").and_then(|v| v.as_array_mut()) {
                hooks_arr.retain(|h| {
                    !h.get("command")
                        .and_then(|v| v.as_str())
                        .is_some_and(is_hcom_codex_command)
                });
            }
        }
        groups_arr.retain(|group| {
            group
                .get("hooks")
                .and_then(|v| v.as_array())
                .is_some_and(|arr| !arr.is_empty())
        });
    }

    hooks_obj.retain(|_, groups| groups.as_array().is_some_and(|arr| !arr.is_empty()));
    if hooks_obj.is_empty() {
        existing.as_object_mut().unwrap().remove("hooks");
    }
}

/// Returns true if `hook` is a legacy hcom Codex entry written in the old
/// `"type":"cmd"` / `"cmd"` format used before Codex 0.129.
fn is_legacy_hcom_codex_cmd_entry(hook: &Value) -> bool {
    hook.get("type").and_then(|v| v.as_str()) == Some("cmd")
        && hook.get("cmd").and_then(|v| v.as_str()).is_some_and(|cmd| {
            CODEX_HOOK_COMMANDS
                .iter()
                .any(|(_, suffix, _)| cmd.ends_with(suffix))
        })
}

/// Remove recognized legacy `"cmd"`-keyed hcom hook entries.
/// Only called when Codex >= CODEX_HOOKS_FEATURE_RENAME_VERSION, which is when
/// the current `"command"`-keyed format is known to be supported.
fn remove_legacy_hcom_cmd_hooks_from_json(existing: &mut Value) {
    let Some(hooks_obj) = existing.get_mut("hooks").and_then(|v| v.as_object_mut()) else {
        return;
    };
    for (_, groups) in hooks_obj.iter_mut() {
        let Some(groups_arr) = groups.as_array_mut() else {
            continue;
        };
        for group in groups_arr.iter_mut() {
            if let Some(hooks_arr) = group.get_mut("hooks").and_then(|v| v.as_array_mut()) {
                hooks_arr.retain(|h| !is_legacy_hcom_codex_cmd_entry(h));
            }
        }
        groups_arr.retain(|group| {
            group
                .get("hooks")
                .and_then(|v| v.as_array())
                .is_some_and(|arr| !arr.is_empty())
        });
    }
    hooks_obj.retain(|_, groups| groups.as_array().is_some_and(|arr| !arr.is_empty()));
    if hooks_obj.is_empty() {
        existing.as_object_mut().unwrap().remove("hooks");
    }
}

fn codex_hook_event_state_label(event: &str) -> &'static str {
    match event {
        "PreToolUse" => "pre_tool_use",
        "PermissionRequest" => "permission_request",
        "PostToolUse" => "post_tool_use",
        "PreCompact" => "pre_compact",
        "PostCompact" => "post_compact",
        "SessionStart" => "session_start",
        "UserPromptSubmit" => "user_prompt_submit",
        "Stop" => "stop",
        _ => "unknown",
    }
}

fn hcom_hook_definition_hash(event: &str, group: &Value, hook: &Value) -> String {
    use sha2::{Digest, Sha256};

    let definition = serde_json::json!({
        "event": event,
        "matcher": group.get("matcher").cloned().unwrap_or(Value::Null),
        "hook": hook,
    });
    let encoded = serde_json::to_vec(&definition).unwrap_or_default();
    let digest = Sha256::digest(&encoded);
    let hex = digest.iter().fold(String::with_capacity(64), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(&mut acc, "{b:02x}");
        acc
    });
    format!("sha256:{hex}")
}

fn hcom_hook_local_entries_from_hooks_json(
    json: &Value,
    hooks_path: &Path,
) -> Vec<CodexHookLocalEntry> {
    let source = hooks_path.to_path_buf();
    let Some(hooks_obj) = json.get("hooks").and_then(|v| v.as_object()) else {
        return Vec::new();
    };

    let mut entries = Vec::new();
    for (event, _, _) in CODEX_HOOK_COMMANDS {
        let Some(groups) = hooks_obj.get(*event).and_then(|v| v.as_array()) else {
            continue;
        };
        for (group_index, group) in groups.iter().enumerate() {
            let Some(hooks) = group.get("hooks").and_then(|v| v.as_array()) else {
                continue;
            };
            for (handler_index, hook) in hooks.iter().enumerate() {
                let Some(command) = hook.get("command").and_then(|v| v.as_str()) else {
                    continue;
                };
                if is_hcom_codex_command(command) {
                    entries.push(CodexHookLocalEntry {
                        key: format!(
                            "{}:{}:{}:{}",
                            source.display(),
                            codex_hook_event_state_label(event),
                            group_index,
                            handler_index
                        ),
                        command: command.to_string(),
                        definition_hash: hcom_hook_definition_hash(event, group, hook),
                    });
                }
            }
        }
    }
    entries
}

fn hcom_hook_state_keys_from_hooks_json(json: &Value, hooks_path: &Path) -> HashSet<String> {
    hcom_hook_local_entries_from_hooks_json(json, hooks_path)
        .into_iter()
        .map(|entry| entry.key)
        .collect()
}

fn hcom_hook_definition_hashes_from_hooks_json(
    json: &Value,
    hooks_path: &Path,
) -> HashMap<String, String> {
    hcom_hook_local_entries_from_hooks_json(json, hooks_path)
        .into_iter()
        .map(|entry| (entry.key, entry.definition_hash))
        .collect()
}

fn hcom_hook_definition_hashes_from_hooks_path(
    hooks_path: &Path,
) -> Result<HashMap<String, String>, VerifyFailReason> {
    let hooks_content = std::fs::read_to_string(hooks_path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            VerifyFailReason::HooksPathMissing(hooks_path.to_path_buf())
        }
        _ => VerifyFailReason::HooksUnreadable(hooks_path.to_path_buf()),
    })?;
    let hooks_json: Value = serde_json::from_str(&hooks_content)
        .map_err(|_| VerifyFailReason::HooksUnreadable(hooks_path.to_path_buf()))?;
    Ok(hcom_hook_definition_hashes_from_hooks_json(
        &hooks_json,
        hooks_path,
    ))
}

fn hcom_hook_local_entries_from_hooks_path(
    hooks_path: &Path,
) -> Result<Vec<CodexHookLocalEntry>, VerifyFailReason> {
    let hooks_content = std::fs::read_to_string(hooks_path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            VerifyFailReason::HooksPathMissing(hooks_path.to_path_buf())
        }
        _ => VerifyFailReason::HooksUnreadable(hooks_path.to_path_buf()),
    })?;
    let hooks_json: Value = serde_json::from_str(&hooks_content)
        .map_err(|_| VerifyFailReason::HooksUnreadable(hooks_path.to_path_buf()))?;
    Ok(hcom_hook_local_entries_from_hooks_json(
        &hooks_json,
        hooks_path,
    ))
}

fn expected_hcom_hook_commands() -> HashSet<String> {
    CODEX_HOOK_COMMANDS
        .iter()
        .map(|(_, command, _)| build_codex_hook_command(command))
        .collect()
}

/// `(hooks.state event label, command)` for each hook hcom installs. Lets tests
/// in other modules build realistic hooks/list responses.
#[cfg(test)]
pub(crate) fn test_expected_hook_specs() -> Vec<(&'static str, String)> {
    CODEX_HOOK_COMMANDS
        .iter()
        .map(|(event, command, _)| {
            (
                codex_hook_event_state_label(event),
                build_codex_hook_command(command),
            )
        })
        .collect()
}

/// Read one string field, accepting the camelCase wire spelling and the
/// snake_case spelling of the core protocol.
fn hook_list_str_field<'a>(hook: &'a Value, camel: &str, snake: &str) -> Option<&'a str> {
    hook.get(camel)
        .or_else(|| hook.get(snake))
        .and_then(|v| v.as_str())
}

fn parse_codex_hook_list_entries(value: &Value) -> Result<Vec<CodexHookListEntry>, String> {
    let hooks = value
        .pointer("/result/data/0/hooks")
        .or_else(|| value.pointer("/data/0/hooks"))
        .or_else(|| value.get("hooks"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| "codex hooks/list response did not contain hooks".to_string())?;

    Ok(hooks
        .iter()
        .map(|hook| CodexHookListEntry {
            key: hook_list_str_field(hook, "key", "key").map(str::to_string),
            command: hook_list_str_field(hook, "command", "command").map(str::to_string),
            source: hook_list_str_field(hook, "source", "source").map(str::to_string),
            source_path: hook_list_str_field(hook, "sourcePath", "source_path").map(PathBuf::from),
            // Codex only treats an explicit `false` as disabled; absent means
            // enabled (default_enabled in the v2 protocol shared module).
            enabled: hook
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            trust_status: hook_list_str_field(hook, "trustStatus", "trust_status")
                .map(str::to_string),
            current_hash: hook_list_str_field(hook, "currentHash", "current_hash")
                .map(str::to_string),
        })
        .collect())
}

/// Whether a `hooks/list` entry is one of hcom's own hook handlers.
///
/// Command equality alone is not identity: any repository can ship a
/// `.codex/hooks.json` containing `{"command": "hcom codex-pretooluse"}`, and a
/// command-only test would let that entry collect hcom's trust state or pass as
/// "already ours" when deciding on the bypass. The entry must also come from the
/// user layer and from hcom's own hooks.json, the single file hcom writes.
fn hook_list_entry_is_hcom_owned(
    entry: &CodexHookListEntry,
    expected_commands: &HashSet<String>,
    hooks_path: &Path,
) -> bool {
    entry
        .command
        .as_deref()
        .is_some_and(|command| expected_commands.contains(command))
        && entry.source.as_deref() == Some(CODEX_HOOK_SOURCE_USER)
        && entry
            .source_path
            .as_deref()
            .is_some_and(|path| paths_equivalent(path, hooks_path))
}

fn describe_hook_list_entry(entry: &CodexHookListEntry) -> String {
    let what = entry
        .command
        .as_deref()
        .or(entry.key.as_deref())
        .unwrap_or("<unnamed hook>");
    match entry.source_path.as_deref() {
        Some(path) => format!("{what} in {}", path.display()),
        None => what.to_string(),
    }
}

/// Hooks that `--dangerously-bypass-hook-trust` would unlock and hcom does not
/// own.
///
/// The flag is invocation-wide for every non-managed hook source — user layer,
/// project layer, and plugins alike (codex-rs/hooks/src/engine/discovery.rs:150,
/// :245, :565-571) — and it also suppresses Codex's own "Hooks need review"
/// prompt (codex-rs/tui/src/startup_hooks_review.rs:245-247). Codex's own help
/// text spells out the contract: "Intended only for automation that already vets
/// hook sources." This is that vetting step, so the flag is only safe when every
/// hook it would newly permit belongs to hcom.
///
/// Note that foreign hooks routinely live in hcom's own hooks.json — hcom merges
/// its entries into whatever file is already there — so the source path alone is
/// never identity.
fn foreign_hooks_unlocked_by_bypass(
    entries: &[CodexHookListEntry],
    hooks_path: &Path,
) -> Vec<String> {
    let expected = expected_hcom_hook_commands();
    entries
        .iter()
        .filter(|entry| {
            // A status hcom does not recognize counts as unlockable: hcom must
            // not reimplement Codex's currentHash algorithm, so it cannot prove
            // such a hook is already trusted.
            let unlockable = entry
                .trust_status
                .as_deref()
                .is_none_or(|status| !CODEX_ALREADY_PERMITTED_TRUST_STATUSES.contains(&status));
            entry.enabled
                && unlockable
                && !hook_list_entry_is_hcom_owned(entry, &expected, hooks_path)
        })
        .map(describe_hook_list_entry)
        .collect()
}

fn hcom_trust_entries_from_hook_list(
    entries: &[CodexHookListEntry],
    hooks_path: &Path,
) -> Result<Vec<CodexHookTrustEntry>, String> {
    let expected = expected_hcom_hook_commands();
    let mut trust_entries = Vec::new();
    for entry in entries {
        if !hook_list_entry_is_hcom_owned(entry, &expected, hooks_path) {
            continue;
        }
        // Ownership implies a command was present.
        let command = entry.command.clone().unwrap_or_default();
        let key = entry
            .key
            .clone()
            .ok_or_else(|| format!("hcom hook {command} missing key"))?;
        let current_hash = entry
            .current_hash
            .clone()
            .ok_or_else(|| format!("hcom hook {command} missing currentHash"))?;
        trust_entries.push(CodexHookTrustEntry {
            key,
            command,
            current_hash,
        });
    }

    let found: HashSet<&str> = trust_entries
        .iter()
        .map(|entry| entry.command.as_str())
        .collect();
    let missing: Vec<String> = expected
        .iter()
        .filter(|command| !found.contains(command.as_str()))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "codex hooks/list missing hcom hooks: {}",
            missing.join(", ")
        ));
    }

    Ok(trust_entries)
}

#[cfg(test)]
fn parse_hcom_hook_entries_from_hooks_list(
    value: &Value,
) -> Result<Vec<CodexHookTrustEntry>, String> {
    let entries = parse_codex_hook_list_entries(value)?;
    hcom_trust_entries_from_hook_list(&entries, &get_codex_hooks_path())
}

/// Synthesize the hooks/list response Codex would return for hcom's own
/// hooks.json, so unit tests exercise the real identity checks without an RPC.
#[cfg(test)]
fn test_hook_list_from_hooks_json(hooks_path: &Path) -> Result<Vec<CodexHookListEntry>, String> {
    let content = std::fs::read_to_string(hooks_path).map_err(|e| e.to_string())?;
    let json: Value = serde_json::from_str(&content).map_err(|e| e.to_string())?;
    let keys = hcom_hook_state_keys_from_hooks_json(&json, hooks_path);
    let commands = expected_hcom_hook_commands();
    if keys.len() != commands.len() {
        return Err(format!(
            "test hooks.json contained {} hcom hook keys, expected {}",
            keys.len(),
            commands.len()
        ));
    }
    let mut keys: Vec<String> = keys.into_iter().collect();
    keys.sort();
    Ok(keys
        .into_iter()
        .enumerate()
        .map(|(index, key)| CodexHookListEntry {
            command: Some(hcom_command_for_hook_state_key(&key)),
            key: Some(key),
            source: Some(CODEX_HOOK_SOURCE_USER.to_string()),
            source_path: Some(hooks_path.to_path_buf()),
            enabled: true,
            trust_status: Some("untrusted".to_string()),
            current_hash: Some(format!("sha256:test-{index}")),
        })
        .collect())
}

fn fetch_codex_hook_list(cwd: &Path) -> Result<Vec<CodexHookListEntry>, String> {
    #[cfg(test)]
    {
        let _ = cwd;
        if let Ok(value) = std::env::var("HCOM_TEST_CODEX_HOOKS_LIST_JSON") {
            if value == "__fail__" {
                return Err("test hook list failure".to_string());
            }
            let json: Value = serde_json::from_str(&value).map_err(|e| e.to_string())?;
            return parse_codex_hook_list_entries(&json);
        }
        test_hook_list_from_hooks_json(&get_codex_hooks_path())
    }

    #[cfg(not(test))]
    {
        let mut child = crate::terminal::executable_command("codex")
            .args(["app-server", "--listen", "stdio://"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to start codex app-server: {e}"))?;

        let stderr_buf = child
            .stderr
            .take()
            .map(spawn_bounded_stderr_reader)
            .unwrap_or_else(|| Arc::new(Mutex::new(String::new())));

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "failed to capture codex app-server stdout".to_string())?;
        let (tx, rx) = mpsc::channel::<String>();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                let _ = tx.send(line);
            }
        });

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "failed to capture codex app-server stdin".to_string())?;
        let initialize = serde_json::json!({
            "method": "initialize",
            "id": 1,
            "params": {
                "clientInfo": {
                    "name": "hcom",
                    "title": "hcom",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": { "experimentalApi": true }
            }
        });
        writeln!(stdin, "{initialize}").map_err(|e| e.to_string())?;
        read_jsonrpc_response(&rx, 1).map_err(|e| with_app_server_stderr(e, &stderr_buf))?;

        writeln!(
            stdin,
            "{}",
            serde_json::json!({"method":"initialized","params":{}})
        )
        .map_err(|e| e.to_string())?;
        let request = serde_json::json!({
            "method": "hooks/list",
            "id": 2,
            "params": { "cwds": [cwd] }
        });
        writeln!(stdin, "{request}").map_err(|e| e.to_string())?;
        stdin.flush().map_err(|e| e.to_string())?;

        let response =
            read_jsonrpc_response(&rx, 2).map_err(|e| with_app_server_stderr(e, &stderr_buf));
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
        parse_codex_hook_list_entries(&response?)
    }
}

fn fetch_codex_hcom_hook_entries(cwd: &Path) -> Result<Vec<CodexHookTrustEntry>, String> {
    let entries = fetch_codex_hook_list(cwd)?;
    hcom_trust_entries_from_hook_list(&entries, &get_codex_hooks_path())
}

#[cfg(not(test))]
fn spawn_bounded_stderr_reader<R>(mut stderr: R) -> Arc<Mutex<String>>
where
    R: Read + Send + 'static,
{
    let buf = Arc::new(Mutex::new(String::new()));
    let thread_buf = Arc::clone(&buf);
    std::thread::spawn(move || {
        let mut chunk = [0_u8; 1024];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&chunk[..n]);
                    let Ok(mut current) = thread_buf.lock() else {
                        break;
                    };
                    let remaining = CODEX_APP_SERVER_STDERR_LIMIT.saturating_sub(current.len());
                    if remaining == 0 {
                        continue;
                    }
                    for ch in text.chars() {
                        if current.len() + ch.len_utf8() > CODEX_APP_SERVER_STDERR_LIMIT {
                            break;
                        }
                        current.push(ch);
                    }
                }
                Err(_) => break,
            }
        }
    });
    buf
}

#[cfg(not(test))]
fn with_app_server_stderr(mut error: String, stderr_buf: &Arc<Mutex<String>>) -> String {
    let stderr = stderr_buf
        .lock()
        .ok()
        .map(|buf| buf.trim().to_string())
        .unwrap_or_default();
    if !stderr.is_empty() {
        error.push_str("; stderr: ");
        error.push_str(&stderr);
    }
    error
}

#[cfg(not(test))]
fn read_jsonrpc_response(rx: &mpsc::Receiver<String>, id: i64) -> Result<Value, String> {
    let deadline = std::time::Instant::now() + CODEX_APP_SERVER_TIMEOUT;
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            return Err(format!(
                "timed out waiting for codex app-server response id {id}"
            ));
        }
        let line = rx
            .recv_timeout(deadline.saturating_duration_since(now))
            .map_err(|e| format!("codex app-server closed before response id {id}: {e}"))?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("id").and_then(|v| v.as_i64()) == Some(id) {
            if let Some(error) = value.get("error") {
                return Err(format!(
                    "codex app-server returned error for id {id}: {error}"
                ));
            }
            return Ok(value);
        }
    }
}

fn parse_codex_cli_version(output: &str) -> Option<(u64, u64, u64)> {
    output
        .split(|c: char| !(c.is_ascii_digit() || c == '.'))
        .find_map(|token| {
            let mut parts = token.split('.');
            let major = parts.next()?.parse().ok()?;
            let minor = parts.next()?.parse().ok()?;
            let patch = parts.next()?.parse().ok()?;
            Some((major, minor, patch))
        })
}

fn codex_cli_version_output_for_hook_trust() -> Result<String, String> {
    #[cfg(test)]
    if let Ok(version) = std::env::var("HCOM_TEST_CODEX_CLI_VERSION") {
        return Ok(version);
    }

    #[cfg(not(test))]
    {
        static CACHE: OnceLock<Result<String, String>> = OnceLock::new();
        CACHE
            .get_or_init(|| {
                let output = crate::terminal::executable_command("codex")
                    .arg("--version")
                    .output()
                    .map_err(|e| {
                        format!("could not run codex --version for hook trust check: {e}")
                    })?;
                let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                Ok(text.trim().to_string())
            })
            .clone()
    }

    #[cfg(test)]
    {
        Err("HCOM_TEST_CODEX_CLI_VERSION not set".to_string())
    }
}

fn codex_hook_trust_version() -> Result<Option<String>, String> {
    let output = codex_cli_version_output_for_hook_trust()?;
    let version = parse_codex_cli_version(&output).ok_or_else(|| {
        format!("could not parse version from codex --version output: {output:?}")
    })?;
    if version >= CODEX_HOOK_TRUST_MIN_VERSION {
        Ok(Some(format!("{}.{}.{}", version.0, version.1, version.2)))
    } else {
        Ok(None)
    }
}

fn codex_hooks_feature_key_for_version(version: (u64, u64, u64)) -> CodexHooksFeatureKey {
    if version >= CODEX_HOOKS_FEATURE_RENAME_VERSION {
        CodexHooksFeatureKey::Hooks
    } else {
        CodexHooksFeatureKey::CodexHooks
    }
}

/// Cached result of `detect_codex_hooks_feature_key`.  Tests bypass the
/// cache when `HCOM_TEST_CODEX_CLI_VERSION` is set so that changing the
/// env var mid-process produces the expected value.
static CODEX_HOOKS_FEATURE_KEY_CACHE: OnceLock<CodexHooksFeatureKey> = OnceLock::new();

fn detect_codex_hooks_feature_key() -> CodexHooksFeatureKey {
    #[cfg(test)]
    if let Ok(version) = std::env::var("HCOM_TEST_CODEX_CLI_VERSION") {
        return parse_codex_cli_version(&version)
            .map(codex_hooks_feature_key_for_version)
            .unwrap_or(CodexHooksFeatureKey::Hooks);
    }

    *CODEX_HOOKS_FEATURE_KEY_CACHE.get_or_init(|| {
        let output = match crate::terminal::executable_command("codex")
            .arg("--version")
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                crate::log::log_warn(
                    "hooks",
                    "codex.version_failed",
                    &format!("could not run codex --version: {e}"),
                );
                return CodexHooksFeatureKey::Hooks;
            }
        };
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        match parse_codex_cli_version(&text) {
            Some(version) => codex_hooks_feature_key_for_version(version),
            None => {
                crate::log::log_warn(
                    "hooks",
                    "codex.version_unparseable",
                    "could not parse version from codex --version output",
                );
                CodexHooksFeatureKey::Hooks
            }
        }
    })
}

fn write_hcom_hook_trust_state(
    config_path: &Path,
    hooks_path: &Path,
    entries: &[CodexHookTrustEntry],
    stale_keys: &HashSet<String>,
    codex_cli_version: &str,
    definition_hashes: &HashMap<String, String>,
) -> Result<(), String> {
    // Defense in depth. `hooks.state` lives in the user's global config.toml and
    // each entry written here both trusts a hook and force-enables it, so a key
    // belonging to any other source — a repo's `.codex/hooks.json`, a plugin —
    // must never reach this table, no matter how the entry was identified
    // upstream. Fail loudly instead of silently skipping: a caller that hands
    // over a foreign key has a bug worth surfacing.
    if let Some(foreign) = entries
        .iter()
        .find(|entry| !hook_state_key_belongs_to_hcom_hooks_json(&entry.key, hooks_path))
    {
        return Err(format!(
            "refusing to write Codex hook trust state for '{}' ({}): key does not belong to hcom's own hooks file {}",
            foreign.key,
            foreign.command,
            hooks_path.display()
        ));
    }

    let mut doc: DocumentMut = if config_path.exists() {
        std::fs::read_to_string(config_path)
            .map_err(|e| e.to_string())?
            .parse::<DocumentMut>()
            .unwrap_or_default()
    } else {
        DocumentMut::new()
    };

    if !doc.contains_table("hooks") {
        doc["hooks"] = Item::Table(toml_edit::Table::new());
    }
    if doc["hooks"]
        .get("state")
        .is_none_or(|item| !item.is_table_like())
    {
        doc["hooks"]["state"] = Item::Table(toml_edit::Table::new());
    }
    let state = doc["hooks"]["state"]
        .as_table_like_mut()
        .ok_or_else(|| "hooks.state config section is not a table".to_string())?;

    for key in stale_keys {
        state.remove(key);
    }

    for entry in entries {
        if state
            .get(&entry.key)
            .is_none_or(|item| !item.is_table_like())
        {
            state.insert(&entry.key, Item::Table(toml_edit::Table::new()));
        }
        let Some(item) = state.get_mut(&entry.key) else {
            continue;
        };
        item["trusted_hash"] = value(entry.current_hash.clone());
        item["enabled"] = value(true);
        item[HCOM_CODEX_CLI_VERSION_KEY] = value(codex_cli_version.to_string());
        if let Some(definition_hash) = definition_hashes.get(&entry.key) {
            item[HCOM_HOOK_DEFINITION_HASH_KEY] = value(definition_hash.clone());
        }
    }

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    paths::atomic_write_io(config_path, &doc.to_string()).map_err(|e| e.to_string())
}

/// Rewrite hcom's own `hooks.state` entries from an authoritative hooks/list
/// inventory, so Codex's `currentHash` values land in the trusted hashes.
fn write_hcom_trust_state_from_hook_list(
    hook_list: &[CodexHookListEntry],
    codex_cli_version: &str,
) -> Result<(), String> {
    let hooks_path = get_codex_hooks_path();
    let entries = hcom_trust_entries_from_hook_list(hook_list, &hooks_path)?;
    let definition_hashes =
        hcom_hook_definition_hashes_from_hooks_path(&hooks_path).map_err(|e| e.to_string())?;
    write_hcom_hook_trust_state(
        &get_codex_config_path(),
        &hooks_path,
        &entries,
        &HashSet::new(),
        codex_cli_version,
        &definition_hashes,
    )
}

/// Decide, once per launch, what hcom may do about Codex's hook-trust gate for a
/// codex started in `launch_dir`.
///
/// Exact trust state is always preferred; the invocation-wide bypass flag is a
/// last resort and is only permitted when hcom can show that nothing but its own
/// hooks would be unlocked by it.
pub(crate) fn resolve_codex_hook_trust_state(launch_dir: &Path) -> CodexHookTrustState {
    let codex_cli_version = match codex_hook_trust_version() {
        // Codex predates the trust gate — nothing is holding hcom's hooks back.
        Ok(None) => return CodexHookTrustState::Trusted,
        Ok(Some(version)) => Some(version),
        // Without a version hcom cannot write valid trust state at all, so treat
        // this exactly like an unavailable hooks/list and decide locally.
        Err(e) => {
            log::log_warn(
                "codex",
                "codex.hook_trust_version_unknown",
                &format!("could not determine Codex version for hook trust: {e}"),
            );
            None
        }
    };

    if let Some(codex_cli_version) = codex_cli_version {
        // This is the launch-time guardrail. Cheap status/verify paths only
        // inspect local metadata, but before opening Codex we ask Codex for
        // authoritative currentHash values and rewrite hcom's trust entries.
        match fetch_codex_hook_list(launch_dir) {
            Ok(hook_list) => {
                match write_hcom_trust_state_from_hook_list(&hook_list, &codex_cli_version) {
                    Ok(()) if codex_hcom_hooks_trusted_locally_for_version(&codex_cli_version) => {
                        return CodexHookTrustState::Trusted;
                    }
                    Ok(()) => log::log_warn(
                        "codex",
                        "codex.hook_trust_self_heal_incomplete",
                        "Codex hook trust self-heal completed but trusted state still looks incomplete",
                    ),
                    Err(e) => log::log_warn(
                        "codex",
                        "codex.hook_trust_self_heal_failed",
                        &format!("Codex hook trust self-heal failed: {e}"),
                    ),
                }

                // Self-heal did not land, but Codex just reported every hook it
                // can see along with its trust status, so the bypass can be
                // judged precisely instead of guessed at.
                let foreign = foreign_hooks_unlocked_by_bypass(&hook_list, &get_codex_hooks_path());
                return if foreign.is_empty() {
                    CodexHookTrustState::BypassSafeFromHooksList
                } else {
                    CodexHookTrustState::BypassUnsafe {
                        reason: format!(
                            "Codex reports enabled but untrusted hooks that are not hcom's: {}",
                            foreign.join(", ")
                        ),
                    }
                };
            }
            Err(e) => {
                // hooks/list is how hcom *refreshes* trust state, not how it
                // checks it. When the RPC is unavailable but the state already
                // on disk is exact for this Codex version, hcom's hooks run on
                // their own and nothing is degraded — going blind here would
                // warn the user and weigh up a bypass that is not needed at all.
                // Worth its own step because a flaky or slow app-server is the
                // ordinary failure here, and it must not turn every launch into
                // a false alarm.
                if codex_hcom_hooks_trusted_locally_for_version(&codex_cli_version) {
                    log::log_warn(
                        "codex",
                        "codex.hook_list_unavailable_state_exact",
                        &format!(
                            "codex hooks/list unavailable, but hcom's persisted hook trust is already exact; launching unchanged: {e}"
                        ),
                    );
                    return CodexHookTrustState::Trusted;
                }
                log::log_warn(
                    "codex",
                    "codex.hook_list_unavailable",
                    &format!(
                        "codex hooks/list unavailable; falling back to a local hook scan: {e}"
                    ),
                );
            }
        }
    }

    // Blind mode: no authoritative inventory. Only bypass when a purely local
    // scan proves that nothing but hcom's own hooks could be in scope.
    match scan_local_codex_hook_definitions(launch_dir) {
        Ok(foreign) if foreign.is_empty() => CodexHookTrustState::BypassSafeFromLocalScan,
        Ok(foreign) => CodexHookTrustState::BypassUnsafe {
            reason: format!(
                "local scan found hook definitions that are not hcom's: {}",
                foreign.join(", ")
            ),
        },
        Err(e) => CodexHookTrustState::BypassUnsafe {
            reason: format!("local hook scan was inconclusive: {e}"),
        },
    }
}

/// Enumerate every Codex hook definition that could be in scope for a launch in
/// `launch_dir` without talking to Codex, and describe each one hcom does not
/// own.
///
/// Covers the three source kinds `--dangerously-bypass-hook-trust` unlocks:
/// - the user layer — `$CODEX_HOME/hooks.json` and a `[hooks]` table in
///   `$CODEX_HOME/config.toml`
/// - project layers — `.codex/hooks.json` and `[hooks]` in `.codex/config.toml`
///   (codex-rs/config/src/loader/mod.rs:1214 `load_project_layers`)
/// - plugins, which hcom cannot resolve into declarations — see
///   `note_possible_plugin_hooks`
///
/// hcom writes exactly one hooks file, so only handlers in that file with a
/// command hcom installs are hcom's; everything found anywhere else is foreign.
/// `Err` means the scan could not be completed and the caller must fail closed.
fn scan_local_codex_hook_definitions(launch_dir: &Path) -> Result<Vec<String>, String> {
    let codex_home = codex_config_dir();
    let hcom_hooks_path = get_codex_hooks_path();
    let expected = expected_hcom_hook_commands();
    let mut foreign = Vec::new();

    collect_foreign_hooks_from_hooks_json(&hcom_hooks_path, &expected, true, &mut foreign)?;
    let user_config_path = codex_home.join("config.toml");
    let user_config = read_toml_value_if_present(&user_config_path)?;
    if let Some(config) = user_config.as_ref() {
        collect_foreign_hooks_from_config_toml(&user_config_path, config, &expected, &mut foreign)?;
        note_declared_plugins(&user_config_path, config, &mut foreign);
    }
    note_possible_plugin_hooks(&codex_home, &mut foreign);

    let markers = codex_project_root_markers(user_config.as_ref())?;
    for dir in codex_project_layer_dirs(launch_dir, &markers)? {
        let dot_codex = dir.join(".codex");
        // Codex skips a project `.codex` that resolves to CODEX_HOME itself
        // (codex-rs/config/src/loader/mod.rs:1256-1259).
        if paths_equivalent(&dot_codex, &codex_home) || !dot_codex.is_dir() {
            continue;
        }
        collect_foreign_hooks_from_hooks_json(
            &dot_codex.join("hooks.json"),
            &expected,
            false,
            &mut foreign,
        )?;
        let config_path = dot_codex.join("config.toml");
        if let Some(config) = read_toml_value_if_present(&config_path)? {
            collect_foreign_hooks_from_config_toml(&config_path, &config, &expected, &mut foreign)?;
            note_declared_plugins(&config_path, &config, &mut foreign);
        }
    }

    Ok(foreign)
}

/// Codex's project-root markers for this machine.
///
/// Codex reads `project_root_markers` from the *merged* config
/// (codex-rs/config/src/loader/mod.rs:305-307); hcom can only see the user layer,
/// so a managed layer overriding the key is out of reach. An explicitly empty
/// array disables root detection, which Codex honors.
fn codex_project_root_markers(user_config: Option<&toml::Value>) -> Result<Vec<String>, String> {
    let Some(markers) = user_config.and_then(|config| config.get("project_root_markers")) else {
        return Ok(CODEX_DEFAULT_PROJECT_ROOT_MARKERS
            .iter()
            .map(|marker| (*marker).to_string())
            .collect());
    };
    markers
        .as_array()
        .ok_or_else(|| "project_root_markers in Codex config.toml is not an array".to_string())?
        .iter()
        .map(|marker| {
            marker.as_str().map(str::to_string).ok_or_else(|| {
                "project_root_markers in Codex config.toml is not an array of strings".to_string()
            })
        })
        .collect()
}

/// Directories whose `.codex` folder Codex would load as a project layer: every
/// directory from the project root down to `launch_dir`
/// (codex-rs/config/src/loader/mod.rs:1214-1235). The project root is the nearest
/// ancestor of `launch_dir` holding one of `markers`, or `launch_dir` itself when
/// no marker is found or the marker list is empty
/// (codex-rs/config/src/loader/mod.rs:1154 `find_project_root`).
fn codex_project_layer_dirs(launch_dir: &Path, markers: &[String]) -> Result<Vec<PathBuf>, String> {
    let mut dirs = Vec::new();
    for ancestor in launch_dir.ancestors() {
        // A `.git` file rather than directory means a linked worktree or a
        // submodule. For linked worktrees Codex reads hook declarations from the
        // *root* checkout's `.codex`, somewhere else on disk entirely
        // (`root_checkout_hooks_folder_for_dir`,
        // codex-rs/config/src/loader/mod.rs:925-935). hcom does not follow that
        // indirection, so the scan cannot claim to be complete.
        if ancestor.join(".git").is_file() {
            return Err(format!(
                "{} is a linked worktree or submodule, so its project hook layer may live in another checkout",
                ancestor.display()
            ));
        }
        dirs.push(ancestor.to_path_buf());
        if markers.is_empty() || markers.iter().any(|marker| ancestor.join(marker).exists()) {
            return Ok(dirs);
        }
    }
    // No marker anywhere up the tree: Codex treats the launch dir as the root.
    Ok(vec![launch_dir.to_path_buf()])
}

fn read_toml_value_if_present(path: &Path) -> Result<Option<toml::Value>, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("could not read {}: {e}", path.display())),
    };
    toml::from_str(&content)
        .map(Some)
        .map_err(|e| format!("could not parse {}: {e}", path.display()))
}

fn collect_foreign_hooks_from_hooks_json(
    path: &Path,
    expected_commands: &HashSet<String>,
    hcom_owns_file: bool,
    out: &mut Vec<String>,
) -> Result<(), String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("could not read {}: {e}", path.display())),
    };
    let json: Value = serde_json::from_str(&content)
        .map_err(|e| format!("could not parse {}: {e}", path.display()))?;
    let Some(events) = json.get("hooks") else {
        return Ok(());
    };
    collect_foreign_hook_events(path, events, expected_commands, hcom_owns_file, out)
}

fn collect_foreign_hooks_from_config_toml(
    path: &Path,
    config: &toml::Value,
    expected_commands: &HashSet<String>,
    out: &mut Vec<String>,
) -> Result<(), String> {
    let Some(events) = config.get("hooks") else {
        return Ok(());
    };
    // A `[hooks]` TOML table has the same shape as the `hooks` object of a
    // hooks.json (both deserialize into `HookEventsToml`,
    // codex-rs/config/src/hook_config.rs:36), so re-encode it and reuse one
    // walker. hcom never writes hook declarations into config.toml, so nothing
    // found here is hcom's.
    let events = serde_json::to_value(events)
        .map_err(|e| format!("could not read [hooks] from {}: {e}", path.display()))?;
    collect_foreign_hook_events(path, &events, expected_commands, false, out)
}

fn collect_foreign_hook_events(
    source: &Path,
    events: &Value,
    expected_commands: &HashSet<String>,
    hcom_owns_file: bool,
    out: &mut Vec<String>,
) -> Result<(), String> {
    let Some(events) = events.as_object() else {
        return Err(format!("hooks in {} is not a table", source.display()));
    };
    for (event, groups) in events {
        // `hooks.state` is trust bookkeeping; only PascalCase event names carry
        // declarations.
        if !CODEX_ALL_HOOK_EVENTS.contains(&event.as_str()) {
            continue;
        }
        let Some(groups) = groups.as_array() else {
            return Err(format!(
                "event '{event}' in {} is not an array of matcher groups",
                source.display()
            ));
        };
        for group in groups {
            let Some(handlers) = group.get("hooks").and_then(|v| v.as_array()) else {
                continue;
            };
            for handler in handlers {
                let command = handler.get("command").and_then(|v| v.as_str());
                // Handlers with no command are Codex's prompt/agent kinds, which
                // hcom cannot inspect — count them as foreign, the fail-closed
                // direction.
                if hcom_owns_file
                    && command.is_some_and(|command| expected_commands.contains(command))
                {
                    continue;
                }
                out.push(format!(
                    "{} in {}",
                    command.unwrap_or("<hook with no command>"),
                    source.display()
                ));
            }
        }
    }
    Ok(())
}

/// A `[plugins]` table in any in-scope layer can activate plugin hook sources.
/// Resolving those into declarations needs each plugin's manifest plus
/// marketplace state (codex-rs/core-plugins/src/loader.rs:199-229), so hcom
/// treats the declaration itself as disqualifying.
fn note_declared_plugins(source: &Path, config: &toml::Value, out: &mut Vec<String>) {
    let declared = config
        .get("plugins")
        .and_then(|plugins| plugins.as_table())
        .is_some_and(|plugins| !plugins.is_empty());
    if declared {
        out.push(format!("[plugins] declared in {}", source.display()));
    }
}

/// Installed plugins can contribute hook sources without appearing in any config
/// file hcom reads, so a non-empty plugin store is disqualifying on its own.
fn note_possible_plugin_hooks(codex_home: &Path, out: &mut Vec<String>) {
    let plugins_root = codex_home.join("plugins");
    let populated = CODEX_PLUGIN_STORE_DIRS.iter().any(|sub| {
        std::fs::read_dir(plugins_root.join(sub)).is_ok_and(|mut entries| entries.next().is_some())
    });
    if populated {
        out.push(format!(
            "installed plugins under {}",
            plugins_root.display()
        ));
    }
}

fn codex_hcom_hooks_trusted_locally_for_version(codex_cli_version: &str) -> bool {
    let hooks_path = get_codex_hooks_path();
    let hooks_content = match std::fs::read_to_string(&hooks_path) {
        Ok(content) => content,
        Err(_) => return false,
    };
    let hooks_json: Value = match serde_json::from_str(&hooks_content) {
        Ok(json) => json,
        Err(_) => return false,
    };
    if verify_hooks_json_value(&hooks_json).is_err() {
        return false;
    }
    let entries = hcom_hook_local_entries_from_hooks_json(&hooks_json, &hooks_path);
    if entries.len() != CODEX_HOOK_COMMANDS.len() {
        return false;
    }
    let definition_hashes: HashMap<String, String> = entries
        .iter()
        .map(|entry| (entry.key.clone(), entry.definition_hash.clone()))
        .collect();
    let keys: HashSet<String> = entries.into_iter().map(|entry| entry.key).collect();

    codex_hcom_hook_keys_trusted_for_version(
        &get_codex_config_path(),
        &keys,
        codex_cli_version,
        &definition_hashes,
    )
}

fn codex_hcom_hook_keys_trusted_for_version(
    config_path: &Path,
    keys: &HashSet<String>,
    codex_cli_version: &str,
    definition_hashes: &HashMap<String, String>,
) -> bool {
    let config_content = match std::fs::read_to_string(config_path) {
        Ok(content) => content,
        Err(_) => return false,
    };
    let doc = match config_content.parse::<DocumentMut>() {
        Ok(doc) => doc,
        Err(_) => return false,
    };
    let Some(state) = doc
        .get("hooks")
        .and_then(|hooks| hooks.get("state"))
        .and_then(|state| state.as_table_like())
    else {
        return false;
    };

    keys.iter().all(|key| {
        let Some(entry) = state.get(key) else {
            return false;
        };
        let Some(trusted_hash) = entry.get("trusted_hash").and_then(|v| v.as_str()) else {
            return false;
        };
        !trusted_hash.is_empty()
            && entry.get("enabled").and_then(|v| v.as_bool()) != Some(false)
            && entry
                .get(HCOM_CODEX_CLI_VERSION_KEY)
                .and_then(|v| v.as_str())
                == Some(codex_cli_version)
            && entry
                .get(HCOM_HOOK_DEFINITION_HASH_KEY)
                .and_then(|v| v.as_str())
                == definition_hashes.get(key).map(String::as_str)
    })
}

#[cfg(test)]
fn hcom_command_for_hook_state_key(key: &str) -> String {
    let mut parts = key.rsplitn(4, ':');
    let _handler_index = parts.next();
    let _group_index = parts.next();
    let event_label = parts.next();
    if let Some(event_label) = event_label {
        for (event, command, _) in CODEX_HOOK_COMMANDS {
            if codex_hook_event_state_label(event) == event_label {
                return build_codex_hook_command(command);
            }
        }
    }
    key.to_string()
}

fn verify_hcom_hook_keys_trusted_for_version(
    config_path: &Path,
    entries: &[CodexHookLocalEntry],
    codex_cli_version: &str,
) -> Result<(), VerifyFailReason> {
    let content = std::fs::read_to_string(config_path)
        .map_err(|e| VerifyFailReason::HookTrustUnavailable(e.to_string()))?;
    let doc = content
        .parse::<DocumentMut>()
        .map_err(|e| VerifyFailReason::HookTrustUnavailable(e.to_string()))?;
    let state = doc
        .get("hooks")
        .and_then(|hooks| hooks.get("state"))
        .and_then(|state| state.as_table_like())
        .ok_or_else(|| VerifyFailReason::HookTrustUnavailable("hooks.state missing".to_string()))?;

    for entry in entries {
        let command = entry.command.clone();
        let Some(state_entry) = state.get(&entry.key) else {
            return Err(VerifyFailReason::HookTrustMissing { command });
        };
        if state_entry.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
            return Err(VerifyFailReason::HookDisabled { command });
        }
        let trusted_hash = state_entry
            .get("trusted_hash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| VerifyFailReason::HookTrustMissing {
                command: command.clone(),
            })?;
        if trusted_hash.is_empty() {
            return Err(VerifyFailReason::HookTrustMissing { command });
        }
        if state_entry
            .get(HCOM_CODEX_CLI_VERSION_KEY)
            .and_then(|v| v.as_str())
            != Some(codex_cli_version)
        {
            return Err(VerifyFailReason::HookTrustStale { command });
        }
        if state_entry
            .get(HCOM_HOOK_DEFINITION_HASH_KEY)
            .and_then(|v| v.as_str())
            != Some(entry.definition_hash.as_str())
        {
            return Err(VerifyFailReason::HookTrustStale { command });
        }
    }

    Ok(())
}

fn verify_hcom_hook_trust_state(
    config_path: &Path,
    hooks_path: &Path,
) -> Result<(), VerifyFailReason> {
    let Some(codex_cli_version) =
        codex_hook_trust_version().map_err(VerifyFailReason::CodexUnavailable)?
    else {
        return Ok(());
    };
    let entries = hcom_hook_local_entries_from_hooks_path(hooks_path)?;
    if entries.len() != CODEX_HOOK_COMMANDS.len() {
        return Err(VerifyFailReason::HookTrustUnavailable(format!(
            "could not derive all hcom hook trust keys from {}",
            hooks_path.display()
        )));
    }

    verify_hcom_hook_keys_trusted_for_version(config_path, &entries, &codex_cli_version)
}

fn ensure_codex_feature_enabled(
    config_path: &Path,
    feature_key: CodexHooksFeatureKey,
) -> Result<(), String> {
    let mut doc: DocumentMut = if config_path.exists() {
        std::fs::read_to_string(config_path)
            .map_err(|e| e.to_string())?
            .parse::<DocumentMut>()
            .unwrap_or_default()
    } else {
        DocumentMut::new()
    };

    if !doc.contains_table("features") {
        doc["features"] = Item::Table(toml_edit::Table::new());
    }
    // Codex renamed the feature flag from codex_hooks to hooks in 0.129.0.
    // Always clean the deprecated codex_hooks key if present; never remove
    // hooks — it's the shared flag for all Codex hooks, not just hcom's.
    remove_codex_hooks_aliases(&mut doc, feature_key);
    doc["features"][feature_key.as_str()] = value(true);
    // Remove the old hcom-owned codex-notify form only; leave unrelated notify untouched.
    let is_hcom_notify = doc.get("notify").is_some_and(is_hcom_legacy_notify);
    if is_hcom_notify {
        doc.remove("notify");
    }

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    if paths::atomic_write(config_path, &doc.to_string()) {
        Ok(())
    } else {
        Err("atomic_write failed".to_string())
    }
}

fn remove_codex_hooks_aliases(doc: &mut DocumentMut, feature_key: CodexHooksFeatureKey) {
    if let Some(features) = doc.get_mut("features")
        && let Some(table) = features.as_table_like_mut()
    {
        table.remove("codex_hooks");
    }

    if feature_key != CodexHooksFeatureKey::Hooks {
        return;
    }

    let Some(profiles) = doc
        .get_mut("profiles")
        .and_then(|item| item.as_table_like_mut())
    else {
        return;
    };
    for (_, profile) in profiles.iter_mut() {
        let Some(features) = profile
            .as_table_like_mut()
            .and_then(|profile| profile.get_mut("features"))
        else {
            continue;
        };
        if let Some(table) = features.as_table_like_mut() {
            table.remove("codex_hooks");
        }
    }
}

fn codex_selected_feature_enabled(config_path: &Path, feature_key: CodexHooksFeatureKey) -> bool {
    let Ok(content) = std::fs::read_to_string(config_path) else {
        return false;
    };
    let Ok(doc) = content.parse::<DocumentMut>() else {
        return false;
    };
    doc.get("features")
        .and_then(|item| item.get(feature_key.as_str()))
        .and_then(|item| item.as_bool())
        .unwrap_or(false)
}

fn codex_deprecated_feature_present(config_path: &Path, feature_key: CodexHooksFeatureKey) -> bool {
    if feature_key != CodexHooksFeatureKey::Hooks {
        return false;
    }
    let Ok(content) = std::fs::read_to_string(config_path) else {
        return false;
    };
    let Ok(doc) = content.parse::<DocumentMut>() else {
        return false;
    };
    if doc
        .get("features")
        .and_then(|item| item.get("codex_hooks"))
        .is_some()
    {
        return true;
    }

    let Some(active_profile) = doc.get("profile").and_then(|item| item.as_str()) else {
        return false;
    };
    doc.get("profiles")
        .and_then(|item| item.as_table_like())
        .and_then(|profiles| profiles.get(active_profile))
        .and_then(|profile| profile.get("features"))
        .and_then(|features| features.get("codex_hooks"))
        .is_some()
}

fn codex_feature_enabled(config_path: &Path, feature_key: CodexHooksFeatureKey) -> bool {
    if codex_selected_feature_enabled(config_path, feature_key) {
        return true;
    }

    let Ok(content) = std::fs::read_to_string(config_path) else {
        return false;
    };
    let Ok(doc) = content.parse::<DocumentMut>() else {
        return false;
    };
    // Check the version-selected key first, fall back to the alternate
    // so that a config written by an older (or newer) hcom still passes
    // verification until the next setup call canonicalizes it.
    doc.get("features")
        .and_then(|item| item.get(feature_key.alternate()))
        .and_then(|item| item.as_bool())
        .unwrap_or(false)
}

/// Whether Codex config already uses the feature flag key expected by the
/// installed Codex CLI. Verification accepts either key for compatibility, but
/// launch setup uses this to self-heal stale `codex_hooks` configs. Modern
/// Codex warns if the deprecated key is present at all, even when `hooks` is
/// also enabled, so treat that mixed state as not current.
pub(crate) fn codex_current_feature_enabled() -> bool {
    let config_path = get_codex_config_path();
    let feature_key = detect_codex_hooks_feature_key();
    codex_selected_feature_enabled(&config_path, feature_key)
        && !codex_deprecated_feature_present(&config_path, feature_key)
}

fn verify_hooks_json_at(hooks_path: &Path) -> Result<(), VerifyFailReason> {
    let content = std::fs::read_to_string(hooks_path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            VerifyFailReason::HooksPathMissing(hooks_path.to_path_buf())
        }
        _ => VerifyFailReason::HooksUnreadable(hooks_path.to_path_buf()),
    })?;
    let json: Value = serde_json::from_str(&content)
        .map_err(|_| VerifyFailReason::HooksUnreadable(hooks_path.to_path_buf()))?;
    verify_hooks_json_value(&json)
}

fn verify_hooks_json_value(json: &Value) -> Result<(), VerifyFailReason> {
    let hooks_obj = json
        .get("hooks")
        .and_then(|v| v.as_object())
        .ok_or(VerifyFailReason::HooksKeyMissing)?;

    // Check all expected hooks are present with correct matchers.
    for (event, command, matcher) in CODEX_HOOK_COMMANDS {
        let groups = match hooks_obj.get(*event).and_then(|v| v.as_array()) {
            Some(arr) if !arr.is_empty() => arr,
            _ => {
                return Err(VerifyFailReason::HookEventMissing {
                    event: (*event).to_string(),
                });
            }
        };
        let expected_command = build_codex_hook_command(command);
        let expected_hook = serde_json::json!({
            "type": "command",
            "command": expected_command,
        });
        // Mirror merge_hcom_hooks: for None-matcher events only match groups
        // with no "matcher" key, not groups with "matcher":"" (which may belong
        // to other tools such as context-mode).
        let matching_group = groups.iter().find(|group| match matcher {
            Some(expected) => group.get("matcher").and_then(|v| v.as_str()) == Some(*expected),
            None => group.get("matcher").and_then(|v| v.as_str()).is_none(),
        });
        let Some(group) = matching_group else {
            return Err(VerifyFailReason::HookCommandMissing {
                event: (*event).to_string(),
                expected_command,
            });
        };
        let hooks = group
            .get("hooks")
            .and_then(|v| v.as_array())
            .ok_or_else(|| VerifyFailReason::HookCommandMissing {
                event: (*event).to_string(),
                expected_command: expected_command.clone(),
            })?;
        let hcom_hooks: Vec<&Value> = hooks
            .iter()
            .filter(|hook| {
                hook.get("command")
                    .and_then(|v| v.as_str())
                    .is_some_and(is_hcom_codex_command)
            })
            .collect();
        if !hcom_hooks.iter().any(|hook| **hook == expected_hook) {
            return Err(VerifyFailReason::HookCommandMissing {
                event: (*event).to_string(),
                expected_command,
            });
        }
        if hcom_hooks.iter().any(|hook| **hook != expected_hook) {
            return Err(VerifyFailReason::HookDefinitionChanged {
                event: (*event).to_string(),
                expected_command,
            });
        }
    }

    // Check no stale hcom hooks exist in groups with non-matching matchers.
    for (event, groups) in hooks_obj {
        let Some(groups) = groups.as_array() else {
            continue;
        };
        for group in groups {
            let has_hcom_command =
                group
                    .get("hooks")
                    .and_then(|v| v.as_array())
                    .is_some_and(|hooks| {
                        hooks.iter().any(|h| {
                            h.get("command")
                                .and_then(|v| v.as_str())
                                .is_some_and(is_hcom_codex_command)
                        })
                    });
            if !has_hcom_command {
                continue;
            }
            // This group has an hcom command — it must match an expected entry.
            let group_matcher = group.get("matcher").and_then(|v| v.as_str());
            let is_expected = CODEX_HOOK_COMMANDS
                .iter()
                .any(|(exp_event, _, exp_matcher)| {
                    *exp_event == event.as_str()
                        && match exp_matcher {
                            Some(m) => group_matcher == Some(*m),
                            None => group_matcher.is_none(),
                        }
                });
            if !is_expected {
                return Err(VerifyFailReason::StaleHcomHookEntry {
                    event: event.clone(),
                    matcher: group_matcher.map(|s| s.to_string()),
                });
            }
        }
    }

    Ok(())
}

fn build_codex_rules() -> String {
    let prefix = crate::runtime_env::get_hcom_prefix();
    let prefix_parts: String = prefix
        .iter()
        .map(|p| format!("\"{}\"", p))
        .collect::<Vec<_>>()
        .join(", ");

    let mut rules = vec!["# hcom integration - auto-approve safe commands".to_string()];
    for cmd in SAFE_HCOM_COMMANDS {
        rules.push(format!(
            "prefix_rule(pattern=[{}, \"{}\"], decision=\"allow\")",
            prefix_parts, cmd
        ));
    }
    for tool in HCOM_TOOL_NAMES {
        rules.push(format!(
            "prefix_rule(pattern=[{}, \"{}\", \"--help\"], decision=\"allow\")",
            prefix_parts, tool
        ));
        rules.push(format!(
            "prefix_rule(pattern=[{}, \"{}\", \"-h\"], decision=\"allow\")",
            prefix_parts, tool
        ));
    }
    rules.join("\n") + "\n"
}

/// Set up Codex execpolicy rules for auto-approval.
pub fn setup_codex_execpolicy() -> bool {
    let rules_dir = get_codex_rules_path();
    let rules_file = rules_dir.join("hcom.rules");
    let rule_content = build_codex_rules();

    if rules_file.exists()
        && std::fs::read_to_string(&rules_file).ok().as_deref() == Some(rule_content.as_str())
    {
        return true;
    }

    let _ = std::fs::create_dir_all(&rules_dir);
    paths::atomic_write(&rules_file, &rule_content)
}

/// Remove hcom execpolicy rule.
pub fn remove_codex_execpolicy() -> bool {
    let rules_file = get_codex_rules_path().join("hcom.rules");
    if rules_file.exists() {
        std::fs::remove_file(&rules_file).is_ok()
    } else {
        true
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum VerifyFailReason {
    #[error("Codex config.toml missing: {}", .0.display())]
    ConfigPathMissing(PathBuf),
    #[error("Codex hooks.json missing: {}", .0.display())]
    HooksPathMissing(PathBuf),
    #[error("Codex experimental hooks feature not enabled in {}", .0.display())]
    CodexFeatureDisabled(PathBuf),
    #[error("Codex hooks.json missing or not parseable as JSON: {}", .0.display())]
    HooksUnreadable(PathBuf),
    #[error("'hooks' key missing or not an object")]
    HooksKeyMissing,
    #[error("hook event '{event}' missing or empty")]
    HookEventMissing { event: String },
    #[error("hcom hook command not found under event '{event}' (expected: {expected_command})")]
    HookCommandMissing {
        event: String,
        expected_command: String,
    },
    #[error("hcom hook definition changed under event '{event}' (expected: {expected_command})")]
    HookDefinitionChanged {
        event: String,
        expected_command: String,
    },
    #[error("stale hcom hook entry in event '{event}' under unexpected matcher: {matcher:?}")]
    StaleHcomHookEntry {
        event: String,
        matcher: Option<String>,
    },
    #[error("Codex CLI unavailable for hook trust check: {0}")]
    CodexUnavailable(String),
    #[error("hcom Codex hook trust state unavailable: {0}")]
    HookTrustUnavailable(String),
    #[error("hcom Codex hook '{command}' has no trusted_hash in hooks.state")]
    HookTrustMissing { command: String },
    #[error("hcom Codex hook '{command}' trusted_hash is stale")]
    HookTrustStale { command: String },
    #[error("hcom Codex hook '{command}' is disabled in hooks.state")]
    HookDisabled { command: String },
    #[error("hcom.rules file missing: {}", .0.display())]
    PermissionsRulesMissing(PathBuf),
}

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error(
        "cannot create a quote-free Codex Windows hook command for the current hcom executable (path contains spaces or cmd metacharacters and has no usable short form): {path}"
    )]
    HookExecutableUnavailable { path: PathBuf },
    #[error("failed to enable Codex experimental hooks feature in {}: {reason}", path.display())]
    EnsureFeatureFailed { path: PathBuf, reason: String },
    #[error("failed to read existing {}: {source}", path.display())]
    HooksReadFailed {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("JSON serialization failed: {0}")]
    SerializationFailed(#[from] serde_json::Error),
    #[error("failed to create parent dir {}: {source}", path.display())]
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
    #[error("post-write verify failed for {}: {reason}", path.display())]
    PostWriteVerifyFailed {
        path: PathBuf,
        #[source]
        reason: VerifyFailReason,
    },
    #[error(
        "failed to trust Codex hooks: {reason}. hcom-wrapped Codex launches may fall back to --dangerously-bypass-hook-trust, but vanilla Codex will not run hcom hooks until trust succeeds"
    )]
    HookTrustFailed { reason: String },
}

pub fn try_setup_codex_hooks(include_permissions: bool) -> Result<(), SetupError> {
    #[cfg(windows)]
    if crate::runtime_env::windows_current_hcom_executable().is_none() {
        return Err(SetupError::HookExecutableUnavailable {
            path: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("<unknown>")),
        });
    }

    let config_path = get_codex_config_path();
    let hooks_path = get_codex_hooks_path();
    let feature_key = detect_codex_hooks_feature_key();

    ensure_codex_feature_enabled(&config_path, feature_key).map_err(|e| {
        SetupError::EnsureFeatureFailed {
            path: config_path.clone(),
            reason: e,
        }
    })?;

    let mut hooks_json = if hooks_path.exists() {
        let content =
            std::fs::read_to_string(&hooks_path).map_err(|source| SetupError::HooksReadFailed {
                path: hooks_path.clone(),
                source,
            })?;
        serde_json::from_str::<Value>(&content)
            .unwrap_or_else(|_| serde_json::json!({ "hooks": {} }))
    } else {
        serde_json::json!({ "hooks": {} })
    };
    // Strip legacy "cmd"-keyed hcom entries written by pre-0.129 installs.
    // Only safe once Codex supports the current "command"-keyed format.
    if feature_key == CodexHooksFeatureKey::Hooks {
        remove_legacy_hcom_cmd_hooks_from_json(&mut hooks_json);
    }
    let old_hcom_hook_keys = hcom_hook_state_keys_from_hooks_json(&hooks_json, &hooks_path);
    merge_hcom_hooks(&mut hooks_json);

    if let Some(parent) = hooks_path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| SetupError::DirCreateFailed {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let content =
        serde_json::to_string_pretty(&hooks_json).map_err(SetupError::SerializationFailed)?;
    paths::atomic_write_io(&hooks_path, &content).map_err(|source| {
        SetupError::AtomicWriteFailed {
            path: hooks_path.clone(),
            source,
        }
    })?;

    verify_hooks_json_at(&hooks_path).map_err(|reason| SetupError::PostWriteVerifyFailed {
        path: hooks_path.clone(),
        reason,
    })?;

    match codex_hook_trust_version() {
        Ok(Some(codex_cli_version)) => {
            let definition_hashes =
                hcom_hook_definition_hashes_from_hooks_json(&hooks_json, &hooks_path);
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            match fetch_codex_hcom_hook_entries(&cwd).and_then(|entries| {
                let current_keys: HashSet<String> =
                    entries.iter().map(|entry| entry.key.clone()).collect();
                let stale_keys: HashSet<String> = old_hcom_hook_keys
                    .difference(&current_keys)
                    .cloned()
                    .collect();
                write_hcom_hook_trust_state(
                    &config_path,
                    &hooks_path,
                    &entries,
                    &stale_keys,
                    &codex_cli_version,
                    &definition_hashes,
                )
            }) {
                Ok(()) => {}
                Err(e) => return Err(SetupError::HookTrustFailed { reason: e }),
            }
        }
        Ok(None) => {}
        Err(e) => log::log_warn(
            "hooks",
            "codex.hook_trust_version_warn",
            &format!(
                "hooks installed but Codex version check failed; launch may fall back to Codex hook-trust bypass: {e}"
            ),
        ),
    }

    let ep_ok = if include_permissions {
        setup_codex_execpolicy()
    } else {
        remove_codex_execpolicy()
    };
    if !ep_ok {
        log::log_warn(
            "hooks",
            "codex.execpolicy_warn",
            "hooks installed but execpolicy write failed; auto-approval will not work",
        );
    }
    Ok(())
}

pub fn setup_codex_hooks(include_permissions: bool) -> bool {
    try_setup_codex_hooks(include_permissions).is_ok()
}

pub fn verify_codex_hooks_installed(check_permissions: bool) -> bool {
    verify_codex_hooks_inner(check_permissions).is_ok()
}

pub(crate) fn verify_codex_hooks_inner(check_permissions: bool) -> Result<(), VerifyFailReason> {
    let config_path = get_codex_config_path();
    let hooks_path = get_codex_hooks_path();

    if !config_path.exists() {
        return Err(VerifyFailReason::ConfigPathMissing(config_path));
    }
    let feature_key = detect_codex_hooks_feature_key();
    if !codex_feature_enabled(&config_path, feature_key) {
        return Err(VerifyFailReason::CodexFeatureDisabled(config_path));
    }
    // No exists() pre-check: verify_hooks_json_at converts NotFound to
    // HooksPathMissing, avoiding a stat-then-open race.
    verify_hooks_json_at(&hooks_path)?;
    verify_hcom_hook_trust_state(&config_path, &hooks_path)?;
    if check_permissions {
        let rules_file = get_codex_rules_path().join("hcom.rules");
        if !rules_file.exists() {
            return Err(VerifyFailReason::PermissionsRulesMissing(rules_file));
        }
    }
    Ok(())
}

/// Remove hcom hooks from a single Codex hooks.json + execpolicy at the given base dir.
fn remove_codex_hooks_from_dir(base: &std::path::Path) -> bool {
    let hooks_path = base.join("hooks.json");
    let rules_file = base.join("rules").join("hcom.rules");
    let mut ok = true;

    if hooks_path.exists() {
        match std::fs::read_to_string(&hooks_path) {
            Ok(content) => {
                let mut json = serde_json::from_str::<Value>(&content)
                    .unwrap_or_else(|_| serde_json::json!({ "hooks": {} }));
                remove_hcom_hooks_from_json(&mut json);
                if json.get("hooks").is_none() && json.as_object().is_some_and(|o| o.is_empty()) {
                    ok &= std::fs::remove_file(&hooks_path).is_ok();
                } else {
                    let content =
                        serde_json::to_string_pretty(&json).unwrap_or_else(|_| "{}".into());
                    ok &= paths::atomic_write(&hooks_path, &content);
                }
            }
            Err(_) => ok = false,
        }
    }

    if rules_file.exists() {
        ok &= std::fs::remove_file(&rules_file).is_ok();
    }

    ok
}

/// Remove hcom hooks from Codex config.
///
/// Cleans the default (~/.codex), env-var (CODEX_HOME), and active HCOM_DIR-local paths.
pub fn remove_codex_hooks() -> bool {
    let default_dir = dirs::home_dir()
        .map(|h| h.join(".codex"))
        .unwrap_or_default();
    let env_dir = std::env::var("CODEX_HOME")
        .ok()
        .filter(|d| !d.is_empty())
        .map(PathBuf::from);
    let local_dir = codex_config_dir();

    let default_ok = remove_codex_hooks_from_dir(&default_dir);
    let env_ok = match env_dir {
        Some(ref d) if *d != default_dir => remove_codex_hooks_from_dir(d),
        _ => true,
    };
    let local_ok = if local_dir != default_dir && Some(&local_dir) != env_dir.as_ref() {
        remove_codex_hooks_from_dir(&local_dir)
    } else {
        true
    };

    default_ok && env_ok && local_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_helpers::{EnvGuard, isolated_test_env};
    use serial_test::serial;

    fn codex_test_ctx(thread_id: &str, cwd: &str) -> HcomContext {
        let mut env = std::env::vars().collect::<HashMap<_, _>>();
        env.remove("HCOM_PROCESS_ID");
        env.insert("CODEX_SANDBOX".to_string(), "1".to_string());
        env.insert("CODEX_THREAD_ID".to_string(), thread_id.to_string());
        env.insert("CODEX_SESSION_ID".to_string(), thread_id.to_string());
        HcomContext::from_env(&env, PathBuf::from(cwd))
    }

    fn codex_test_payload(event: &str, session_id: &str, cwd: &str) -> HookPayload {
        HookPayload::from_codex_native(
            event,
            serde_json::json!({
                "session_id": session_id,
                "cwd": cwd,
            }),
        )
    }

    fn log_codex_stopped_snapshot(
        db: &HcomDb,
        name: &str,
        session_id: &str,
        tool: &str,
        cwd: &str,
        last_event_id: i64,
    ) {
        db.log_event(
            "life",
            name,
            &serde_json::json!({
                "action": "stopped",
                "snapshot": {
                    "name": name,
                    "session_id": session_id,
                    "tool": tool,
                    "directory": cwd,
                    "transcript_path": null,
                    "parent_name": null,
                    "parent_session_id": null,
                    "tag": "desktop",
                    "wait_timeout": 321,
                    "subagent_timeout": null,
                    "hints": "restored",
                    "background": 0,
                    "agent_id": null,
                    "name_announced": 1,
                    "origin_device_id": null,
                    "last_event_id": last_event_id
                }
            }),
        )
        .unwrap();
    }

    #[test]
    #[serial]
    fn test_sessionstart_restores_stopped_desktop_without_process_binding() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        let session_id = "thread-restored";
        let name = "desktop-restored";
        let cwd = "/tmp/project";

        instance_binding::initialize_instance_in_position_file(
            &db,
            name,
            Some(session_id),
            None,
            None,
            None,
            None,
            Some("codex"),
            false,
            Some("desktop"),
            Some(321),
            None,
            Some("restored"),
            Some(cwd),
        );
        db.set_session_binding(session_id, name).unwrap();
        log_codex_stopped_snapshot(&db, name, session_id, "codex", cwd, 27);
        db.delete_instance(name).unwrap();
        assert!(db.get_instance_full(name).unwrap().is_none());
        assert!(db.get_session_binding(session_id).unwrap().is_none());

        let ctx = codex_test_ctx(session_id, cwd);
        let payload = codex_test_payload("SessionStart", session_id, cwd);
        let _ = handle_sessionstart(&db, &ctx, &payload);

        let row = db
            .get_instance_full(name)
            .unwrap()
            .expect("SessionStart must recreate the deleted Desktop row");
        assert_eq!(row.tool, "codex");
        assert_eq!(row.directory, cwd);
        assert_eq!(row.session_id.as_deref(), Some(session_id));
        assert_eq!(
            row.last_event_id, 27,
            "delivery cursor must survive restore"
        );
        assert_eq!(row.tag.as_deref(), Some("desktop"));
        assert_eq!(row.hints.as_deref(), Some("restored"));
        assert_eq!(row.wait_timeout, Some(321));
        assert_eq!(
            db.get_session_binding(session_id).unwrap().as_deref(),
            Some(name)
        );
    }

    #[test]
    #[serial]
    fn test_sessionstart_refuses_unpinned_cross_directory_restore() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        let session_id = "thread-relocated";
        let name = "cultivation";
        let old_cwd = "/tmp/old-cultivation-project";
        let new_cwd = "/tmp/cultivation";

        log_codex_stopped_snapshot(&db, name, session_id, "codex", old_cwd, 81);
        let ctx = codex_test_ctx(session_id, new_cwd);
        let payload = codex_test_payload("SessionStart", session_id, new_cwd);
        let _ = handle_sessionstart(&db, &ctx, &payload);

        assert!(db.get_instance_full(name).unwrap().is_none());
        assert!(db.get_session_binding(session_id).unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_sessionstart_restores_pinned_directory_despite_native_cwd() {
        let (tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        let session_id = "thread-pinned";
        let name = "cultivation";
        let native_cwd = tmp.path().join("old-scaffold");
        let pinned_cwd = tmp.path().join("cultivation");
        std::fs::create_dir_all(&native_cwd).unwrap();
        std::fs::create_dir_all(&pinned_cwd).unwrap();
        let native_cwd = native_cwd.to_string_lossy().to_string();
        let pinned_cwd = pinned_cwd.to_string_lossy().to_string();
        let pin_context = directory_override_launch_context(&pinned_cwd, session_id);
        let pin_value: Value = serde_json::from_str(&pin_context).unwrap();

        db.log_event(
            "life",
            name,
            &serde_json::json!({
                "action": "stopped",
                "snapshot": {
                    "name": name,
                    "session_id": session_id,
                    "tool": "codex",
                    "directory": pinned_cwd,
                    "transcript_path": null,
                    "parent_name": null,
                    "parent_session_id": null,
                    "tag": "desktop",
                    "wait_timeout": 321,
                    "subagent_timeout": null,
                    "hints": "restored",
                    "background": 0,
                    "agent_id": null,
                    "name_announced": 1,
                    "origin_device_id": null,
                    "last_event_id": 81,
                    "codex_directory_pin_v1": pin_value[CODEX_DIRECTORY_PIN_KEY].clone()
                }
            }),
        )
        .unwrap();

        let ctx = codex_test_ctx(session_id, &native_cwd);
        let payload = codex_test_payload("SessionStart", session_id, &native_cwd);
        let _ = handle_sessionstart(&db, &ctx, &payload);

        let row = db
            .get_instance_full(name)
            .unwrap()
            .expect("a valid stopped pin must restore the logical directory");
        assert_eq!(row.directory, pinned_cwd);
        assert_eq!(row.session_id.as_deref(), Some(session_id));
        assert_eq!(row.last_event_id, 81);
        assert_eq!(
            pinned_directory_from_launch_context(row.launch_context.as_deref(), session_id)
                .as_deref(),
            Some(pinned_cwd.as_str())
        );

        let later = codex_test_payload("UserPromptSubmit", session_id, &native_cwd);
        let _ = handle_userpromptsubmit(&db, &ctx, &later);
        assert_eq!(
            db.get_instance_full(name).unwrap().unwrap().directory,
            pinned_cwd,
            "ordinary hooks must not overwrite the logical directory pin"
        );
    }

    #[test]
    #[serial]
    fn test_directory_pin_survives_real_stop_and_sessionstart_cycle() {
        let (tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        let session_id = "thread-pin-cycle";
        let name = "cultivation";
        let native_cwd = tmp.path().join("old-scaffold");
        let pinned_cwd = tmp.path().join("cultivation");
        std::fs::create_dir_all(&native_cwd).unwrap();
        std::fs::create_dir_all(&pinned_cwd).unwrap();
        let native_cwd = native_cwd.to_string_lossy().to_string();
        let pinned_cwd = pinned_cwd.to_string_lossy().to_string();

        assert!(instance_binding::initialize_instance_in_position_file(
            &db,
            name,
            Some(session_id),
            None,
            None,
            None,
            None,
            Some("codex"),
            false,
            Some("desktop"),
            Some(321),
            None,
            Some("relocated"),
            Some(&pinned_cwd),
        ));
        db.store_launch_context(
            name,
            &directory_override_launch_context(&pinned_cwd, session_id),
        )
        .unwrap();
        db.set_session_binding(session_id, name).unwrap();

        common::stop_instance(&db, name, "test", "pin-cycle");
        assert!(db.get_instance_full(name).unwrap().is_none());

        let ctx = codex_test_ctx(session_id, &native_cwd);
        let payload = codex_test_payload("SessionStart", session_id, &native_cwd);
        let _ = handle_sessionstart(&db, &ctx, &payload);

        let row = db.get_instance_full(name).unwrap().unwrap();
        assert_eq!(row.directory, pinned_cwd);
        assert_eq!(row.session_id.as_deref(), Some(session_id));
        assert_eq!(
            pinned_directory_from_launch_context(row.launch_context.as_deref(), session_id)
                .as_deref(),
            Some(pinned_cwd.as_str())
        );
    }

    #[test]
    #[serial]
    fn test_sessionstart_refuses_older_pinned_snapshot_after_newer_owner_stop() {
        let (tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        let name = "cultivation";
        let old_session = "thread-old-owner";
        let new_session = "thread-new-owner";
        let native_cwd = tmp.path().join("old-scaffold");
        let pinned_cwd = tmp.path().join("cultivation");
        let newer_cwd = tmp.path().join("new-owner");
        std::fs::create_dir_all(&native_cwd).unwrap();
        std::fs::create_dir_all(&pinned_cwd).unwrap();
        std::fs::create_dir_all(&newer_cwd).unwrap();
        let native_cwd = native_cwd.to_string_lossy().to_string();
        let pinned_cwd = pinned_cwd.to_string_lossy().to_string();
        let newer_cwd = newer_cwd.to_string_lossy().to_string();
        let pin_context = directory_override_launch_context(&pinned_cwd, old_session);
        let pin_value: Value = serde_json::from_str(&pin_context).unwrap();

        db.log_event(
            "life",
            name,
            &serde_json::json!({
                "action": "stopped",
                "snapshot": {
                    "session_id": old_session,
                    "tool": "codex",
                    "directory": pinned_cwd,
                    "origin_device_id": null,
                    "last_event_id": 10,
                    "codex_directory_pin_v1": pin_value[CODEX_DIRECTORY_PIN_KEY].clone()
                }
            }),
        )
        .unwrap();
        db.log_event(
            "life",
            name,
            &serde_json::json!({
                "action": "stopped",
                "snapshot": {
                    "session_id": new_session,
                    "tool": "codex",
                    "directory": newer_cwd,
                    "origin_device_id": null,
                    "last_event_id": 11
                }
            }),
        )
        .unwrap();

        let ctx = codex_test_ctx(old_session, &native_cwd);
        let payload = codex_test_payload("SessionStart", old_session, &native_cwd);
        let _ = handle_sessionstart(&db, &ctx, &payload);

        assert!(db.get_instance_full(name).unwrap().is_none());
        assert!(db.get_session_binding(old_session).unwrap().is_none());
        assert!(db.get_session_binding(new_session).unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_sessionstart_refuses_pin_whose_directory_is_missing_or_file() {
        let (tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();

        for (index, replacement_is_file) in [false, true].into_iter().enumerate() {
            let name = format!("cultivation-{index}");
            let session_id = format!("thread-dead-pin-{index}");
            let native_cwd = tmp.path().join(format!("native-{index}"));
            let pinned_cwd = tmp.path().join(format!("pin-{index}"));
            std::fs::create_dir_all(&native_cwd).unwrap();
            std::fs::create_dir_all(&pinned_cwd).unwrap();
            let native_cwd = native_cwd.to_string_lossy().to_string();
            let pinned_cwd = pinned_cwd.to_string_lossy().to_string();
            let pin_context = directory_override_launch_context(&pinned_cwd, &session_id);
            let pin_value: Value = serde_json::from_str(&pin_context).unwrap();
            db.log_event(
                "life",
                &name,
                &serde_json::json!({
                    "action": "stopped",
                    "snapshot": {
                        "session_id": session_id,
                        "tool": "codex",
                        "directory": pinned_cwd,
                        "origin_device_id": null,
                        "last_event_id": 12,
                        "codex_directory_pin_v1": pin_value[CODEX_DIRECTORY_PIN_KEY].clone()
                    }
                }),
            )
            .unwrap();
            std::fs::remove_dir(&pinned_cwd).unwrap();
            if replacement_is_file {
                std::fs::write(&pinned_cwd, b"not a directory").unwrap();
            }

            let ctx = codex_test_ctx(&session_id, &native_cwd);
            let payload = codex_test_payload("SessionStart", &session_id, &native_cwd);
            let _ = handle_sessionstart(&db, &ctx, &payload);

            assert!(db.get_instance_full(&name).unwrap().is_none());
            assert!(db.get_session_binding(&session_id).unwrap().is_none());
        }
    }

    #[test]
    #[serial]
    fn test_sessionstart_refuses_missing_or_mismatched_context_identity() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        let session_id = "thread-owned";
        let name = "cultivation";
        let cwd = "/tmp/cultivation";
        log_codex_stopped_snapshot(&db, name, session_id, "codex", "/tmp/old", 0);

        let missing_ctx = codex_test_ctx("", cwd);
        let payload = codex_test_payload("SessionStart", session_id, cwd);
        let _ = handle_sessionstart(&db, &missing_ctx, &payload);
        assert!(db.get_instance_full(name).unwrap().is_none());

        let mismatched_ctx = codex_test_ctx("thread-foreign", cwd);
        let _ = handle_sessionstart(&db, &mismatched_ctx, &payload);
        assert!(db.get_instance_full(name).unwrap().is_none());

        let mut mismatched_session_env = codex_test_ctx(session_id, cwd);
        mismatched_session_env
            .raw_env
            .insert("CODEX_SESSION_ID".into(), "thread-foreign".into());
        let _ = handle_sessionstart(&db, &mismatched_session_env, &payload);
        assert!(db.get_instance_full(name).unwrap().is_none());
        assert!(db.get_session_binding(session_id).unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_sessionstart_refuses_wrong_snapshot_session() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        let name = "cultivation";
        let cwd = "/tmp/cultivation";
        log_codex_stopped_snapshot(&db, name, "thread-original", "codex", "/tmp/old", 0);

        let ctx = codex_test_ctx("thread-other", cwd);
        let payload = codex_test_payload("SessionStart", "thread-other", cwd);
        let _ = handle_sessionstart(&db, &ctx, &payload);

        assert!(db.get_instance_full(name).unwrap().is_none());
        assert!(db.get_session_binding("thread-original").unwrap().is_none());
        assert!(db.get_session_binding("thread-other").unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_sessionstart_repairs_missing_binding_for_existing_codex_row() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        let session_id = "thread-unbound";
        let name = "desktop-unbound";
        let cwd = "/tmp/project";
        instance_binding::initialize_instance_in_position_file(
            &db,
            name,
            Some(session_id),
            None,
            None,
            None,
            None,
            Some("codex"),
            false,
            None,
            None,
            None,
            None,
            Some(cwd),
        );
        log_codex_stopped_snapshot(&db, name, session_id, "codex", cwd, 0);
        assert!(db.get_session_binding(session_id).unwrap().is_none());

        let ctx = codex_test_ctx(session_id, cwd);
        let payload = codex_test_payload("SessionStart", session_id, cwd);
        let _ = handle_sessionstart(&db, &ctx, &payload);

        assert_eq!(
            db.get_session_binding(session_id).unwrap().as_deref(),
            Some(name)
        );
        assert!(db.get_instance_full(name).unwrap().is_some());
    }

    #[test]
    #[serial]
    fn test_non_sessionstart_hook_does_not_restore_intentionally_stopped_codex() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        let session_id = "thread-stopped";
        let name = "desktop-stopped";
        let cwd = "/tmp/project";
        log_codex_stopped_snapshot(&db, name, session_id, "codex", cwd, 0);

        let ctx = codex_test_ctx(session_id, cwd);
        let payload = codex_test_payload("UserPromptSubmit", session_id, cwd);
        let _ = handle_userpromptsubmit(&db, &ctx, &payload);

        assert!(db.get_instance_full(name).unwrap().is_none());
        assert!(db.get_session_binding(session_id).unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_sessionstart_refuses_incompatible_stopped_snapshot() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let db = HcomDb::open().unwrap();
        let cwd = "/tmp/project";

        let name = "wrong-tool";
        let session_id = "thread-wrong-tool";
        log_codex_stopped_snapshot(&db, name, session_id, "claude", cwd, 0);
        let ctx = codex_test_ctx(session_id, cwd);
        let payload = codex_test_payload("SessionStart", session_id, cwd);
        let _ = handle_sessionstart(&db, &ctx, &payload);

        assert!(
            db.get_instance_full(name).unwrap().is_none(),
            "incompatible stopped identity {name} must not be recreated"
        );
        assert!(db.get_session_binding(session_id).unwrap().is_none());

        let live_name = "live-wrong-tool";
        let live_session = "thread-live-wrong-tool";
        instance_binding::initialize_instance_in_position_file(
            &db,
            live_name,
            Some(live_session),
            None,
            None,
            None,
            None,
            Some("claude"),
            false,
            None,
            None,
            None,
            None,
            Some(cwd),
        );
        db.set_session_binding(live_session, live_name).unwrap();
        let ctx = codex_test_ctx(live_session, cwd);
        let payload = codex_test_payload("SessionStart", live_session, cwd);
        let _ = handle_sessionstart(&db, &ctx, &payload);
        assert_eq!(
            db.get_instance_full(live_name).unwrap().unwrap().tool,
            "claude",
            "a live non-Codex binding must not be claimed"
        );

        let live_codex_name = "live-wrong-directory";
        let live_codex_session = "thread-live-wrong-directory";
        instance_binding::initialize_instance_in_position_file(
            &db,
            live_codex_name,
            Some(live_codex_session),
            None,
            None,
            None,
            None,
            Some("codex"),
            false,
            None,
            None,
            None,
            None,
            Some("/tmp/other-project"),
        );
        db.set_session_binding(live_codex_session, live_codex_name)
            .unwrap();
        let ctx = codex_test_ctx(live_codex_session, cwd);
        let payload = codex_test_payload("SessionStart", live_codex_session, cwd);
        let _ = handle_sessionstart(&db, &ctx, &payload);
        let row = db.get_instance_full(live_codex_name).unwrap().unwrap();
        assert_eq!(row.directory, "/tmp/other-project");
        assert_eq!(row.session_id.as_deref(), Some(live_codex_session));
    }

    #[test]
    fn test_hook_payload_factory_uses_native_fields() {
        let payload = HookPayload::from_codex_native(
            "UserPromptSubmit",
            serde_json::json!({
                "session_id": "sess-1",
                "prompt": "<hcom>",
            }),
        );
        assert_eq!(payload.session_id.as_deref(), Some("sess-1"));
        assert_eq!(payload.hook_name, "UserPromptSubmit");
    }

    #[test]
    fn test_derive_transcript_empty_thread_id() {
        assert!(derive_codex_transcript_path("").is_none());
    }

    #[test]
    fn test_derive_transcript_no_match() {
        assert!(derive_codex_transcript_path("nonexistent-thread-12345").is_none());
    }

    #[test]
    fn test_normalize_transcript_path() {
        assert_eq!(
            normalize_codex_transcript_path("C:\\Users\\runner\\session.jsonl"),
            "C:\\Users\\runner\\session.jsonl"
        );
        assert_eq!(
            normalize_codex_transcript_path("\\\\?\\C:\\Users\\runner\\session.jsonl"),
            "C:\\Users\\runner\\session.jsonl"
        );
        assert_eq!(
            normalize_codex_transcript_path("\\\\?\\UNC\\server\\share\\session.jsonl"),
            "\\\\server\\share\\session.jsonl"
        );
    }

    #[test]
    #[serial]
    fn test_derive_transcript_finds_file() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions").join("project");
        std::fs::create_dir_all(&sessions).unwrap();

        let transcript = sessions.join("rollout-1-abc-123-def.jsonl");
        std::fs::File::create(&transcript).unwrap();

        let saved = std::env::var("CODEX_HOME").ok();
        unsafe { std::env::set_var("CODEX_HOME", dir.path()) };

        let result = derive_codex_transcript_path("abc-123-def");
        assert!(result.is_some(), "should find transcript file");
        assert!(result.unwrap().contains("rollout-1-abc-123-def.jsonl"));

        if let Some(v) = saved {
            unsafe { std::env::set_var("CODEX_HOME", v) };
        } else {
            unsafe { std::env::remove_var("CODEX_HOME") };
        }
    }

    // -- build_codex_rules --

    #[test]
    fn test_build_codex_rules_contains_send() {
        let rules = build_codex_rules();
        assert!(rules.contains("\"send\""));
        assert!(rules.contains("\"list\""));
        assert!(rules.contains("decision=\"allow\""));
    }

    #[test]
    fn test_build_codex_rules_contains_tool_help() {
        let rules = build_codex_rules();
        assert!(rules.contains("\"claude\", \"--help\""));
        assert!(rules.contains("\"gemini\", \"-h\""));
    }

    // -- settings setup/remove/verify --

    #[test]
    #[cfg(windows)]
    fn test_pinned_windows_codex_hook_command_is_quote_free() {
        assert_eq!(
            build_pinned_windows_codex_hook_command(
                "C:/Users/TestUser/.hcom/bin/hcom.exe",
                "codex-stop",
            ),
            "C:/Users/TestUser/.hcom/bin/hcom.exe codex-stop"
        );
    }

    #[test]
    #[cfg(windows)]
    #[serial]
    fn test_setup_codex_hooks_pins_current_executable_without_bare_hcom() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        unsafe { std::env::set_var("HCOM_TEST_CODEX_CLI_VERSION", "codex-cli 0.130.0") };

        assert!(setup_codex_hooks(false));
        let json: Value = serde_json::from_str(
            &std::fs::read_to_string(get_codex_hooks_path()).expect("read generated hooks"),
        )
        .expect("parse generated hooks");
        let expected_executable = crate::runtime_env::windows_current_hcom_executable()
            .expect("current executable should resolve");
        let expected_prefix = format!("{expected_executable} ");

        for (event, _, _) in CODEX_HOOK_COMMANDS {
            let groups = json["hooks"][*event]
                .as_array()
                .unwrap_or_else(|| panic!("{event} groups missing"));
            let commands: Vec<&str> = groups
                .iter()
                .flat_map(|group| {
                    group["hooks"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|hook| hook["command"].as_str())
                })
                .filter(|command| is_hcom_codex_command(command))
                .collect();
            assert_eq!(commands.len(), 1, "{event} generated hook count");
            assert!(
                commands[0].starts_with(&expected_prefix),
                "{event} did not pin current executable: {}",
                commands[0]
            );
            assert!(!commands[0].starts_with("hcom "));
            assert!(!commands[0].contains("${HCOM"));
            assert!(!commands[0].contains('"'));
        }
    }

    #[test]
    #[serial]
    fn test_setup_codex_hooks_reinstall_is_idempotent() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        unsafe { std::env::set_var("HCOM_TEST_CODEX_CLI_VERSION", "codex-cli 0.130.0") };

        assert!(setup_codex_hooks(false));
        let first = std::fs::read_to_string(get_codex_hooks_path()).unwrap();
        assert!(setup_codex_hooks(false));
        let second = std::fs::read_to_string(get_codex_hooks_path()).unwrap();

        assert_eq!(first, second, "Codex hook reinstall changed hooks.json");
        assert!(verify_codex_hooks_installed(false));
    }

    #[test]
    #[serial]
    fn test_setup_and_remove_codex_hooks() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        unsafe { std::env::set_var("HCOM_TEST_CODEX_CLI_VERSION", "codex-cli 0.130.0") };
        assert!(setup_codex_hooks(false));
        assert!(verify_codex_hooks_installed(false));

        let hooks_path = get_codex_hooks_path();
        let config_path = get_codex_config_path();
        let hooks_content = std::fs::read_to_string(hooks_path).unwrap();
        let config_content = std::fs::read_to_string(config_path).unwrap();

        assert!(hooks_content.contains("codex-sessionstart"));
        assert!(config_content.contains("hooks = true"));
        assert!(!config_content.contains("codex-notify"));

        assert!(remove_codex_hooks());
        assert!(!verify_codex_hooks_installed(false));
    }

    #[test]
    #[serial]
    fn test_setup_codex_hooks_trusts_hcom_hooks_for_modern_codex() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        unsafe { std::env::set_var("HCOM_TEST_CODEX_CLI_VERSION", "codex-cli 0.131.0") };

        assert!(setup_codex_hooks(false));
        assert!(verify_codex_hooks_installed(false));

        let config_content = std::fs::read_to_string(get_codex_config_path()).unwrap();
        assert!(config_content.contains("trusted_hash"));
        assert!(config_content.contains("enabled = true"));
        assert!(config_content.contains("hcom_codex_cli_version = \"0.131.0\""));
        assert!(config_content.contains("hcom_hook_definition_hash"));
    }

    #[test]
    #[serial]
    fn test_setup_codex_hooks_repairs_disabled_hcom_hook_state() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        unsafe { std::env::set_var("HCOM_TEST_CODEX_CLI_VERSION", "codex-cli 0.131.0") };

        assert!(setup_codex_hooks(false));

        let config_path = get_codex_config_path();
        let content = std::fs::read_to_string(&config_path).unwrap();
        let mut doc = content.parse::<DocumentMut>().unwrap();
        let state = doc["hooks"]["state"].as_table_like_mut().unwrap();
        let first_key = state.iter().next().unwrap().0.to_string();
        state.get_mut(&first_key).unwrap()["enabled"] = value(false);
        paths::atomic_write_io(&config_path, &doc.to_string()).unwrap();

        assert!(!verify_codex_hooks_installed(false));

        assert!(setup_codex_hooks(false));
        assert!(verify_codex_hooks_installed(false));
        let repaired = std::fs::read_to_string(&config_path).unwrap();
        let repaired_doc = repaired.parse::<DocumentMut>().unwrap();
        assert_eq!(
            repaired_doc["hooks"]["state"][&first_key]["enabled"].as_bool(),
            Some(true)
        );
    }

    #[test]
    #[serial]
    fn test_setup_codex_hooks_repairs_stale_trusted_hash() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        unsafe { std::env::set_var("HCOM_TEST_CODEX_CLI_VERSION", "codex-cli 0.131.0") };

        assert!(setup_codex_hooks(false));
        assert!(verify_codex_hooks_installed(false));

        let config_path = get_codex_config_path();
        let content = std::fs::read_to_string(&config_path).unwrap();
        let mut doc = content.parse::<DocumentMut>().unwrap();
        let state = doc["hooks"]["state"].as_table_like_mut().unwrap();
        let first_key = state.iter().next().unwrap().0.to_string();
        state.get_mut(&first_key).unwrap()["trusted_hash"] = value("sha256:stale");
        paths::atomic_write_io(&config_path, &doc.to_string()).unwrap();

        // Cheap verify does not spawn Codex app-server to compare currentHash.
        assert!(verify_codex_hooks_installed(false));

        assert!(setup_codex_hooks(false));
        assert!(verify_codex_hooks_installed(false));
        let repaired = std::fs::read_to_string(&config_path).unwrap();
        let repaired_doc = repaired.parse::<DocumentMut>().unwrap();
        assert_ne!(
            repaired_doc["hooks"]["state"][&first_key]["trusted_hash"].as_str(),
            Some("sha256:stale")
        );
    }

    #[test]
    #[serial]
    fn test_setup_codex_hooks_repairs_version_stamped_trust_state() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        unsafe { std::env::set_var("HCOM_TEST_CODEX_CLI_VERSION", "codex-cli 0.131.0") };

        assert!(setup_codex_hooks(false));
        assert!(verify_codex_hooks_installed(false));

        unsafe { std::env::set_var("HCOM_TEST_CODEX_CLI_VERSION", "codex-cli 0.132.0") };
        assert!(!verify_codex_hooks_installed(false));

        assert!(setup_codex_hooks(false));
        assert!(verify_codex_hooks_installed(false));
        let repaired = std::fs::read_to_string(get_codex_config_path()).unwrap();
        assert!(repaired.contains("hcom_codex_cli_version = \"0.132.0\""));
    }

    #[test]
    #[serial]
    fn test_setup_codex_hooks_repairs_drifted_hcom_hook_definition() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        unsafe { std::env::set_var("HCOM_TEST_CODEX_CLI_VERSION", "codex-cli 0.131.0") };

        assert!(setup_codex_hooks(false));
        assert!(verify_codex_hooks_installed(false));

        let hooks_path = get_codex_hooks_path();
        let content = std::fs::read_to_string(&hooks_path).unwrap();
        let mut json: Value = serde_json::from_str(&content).unwrap();
        json["hooks"]["PreToolUse"][0]["hooks"][0]["statusMessage"] =
            Value::String("running".to_string());
        paths::atomic_write_io(&hooks_path, &serde_json::to_string_pretty(&json).unwrap()).unwrap();

        assert!(!verify_codex_hooks_installed(false));

        assert!(setup_codex_hooks(false));
        assert!(verify_codex_hooks_installed(false));
        let repaired: Value =
            serde_json::from_str(&std::fs::read_to_string(&hooks_path).unwrap()).unwrap();
        assert!(
            repaired["hooks"]["PreToolUse"][0]["hooks"][0]
                .get("statusMessage")
                .is_none()
        );
    }

    // ── GHSA-pwv3-8r7h-p373: hook identity must be source-scoped ────────────

    /// hooks/list entries for hcom's own five hooks, plus whatever `extra` adds.
    fn hooks_list_value(hooks_path: &Path, extra: Vec<Value>) -> Value {
        let mut hooks: Vec<Value> = test_expected_hook_specs()
            .into_iter()
            .enumerate()
            .map(|(index, (event_label, command))| {
                serde_json::json!({
                    "key": format!("{}:{event_label}:0:0", hooks_path.display()),
                    "command": command,
                    "source": "user",
                    "sourcePath": hooks_path.to_string_lossy(),
                    "enabled": true,
                    "trustStatus": "untrusted",
                    "currentHash": format!("sha256:list-{index}"),
                })
            })
            .collect();
        hooks.extend(extra);
        serde_json::json!({ "result": { "data": [{ "hooks": hooks }] } })
    }

    #[test]
    #[serial]
    fn test_impersonating_project_hook_gets_no_trust_entry() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let hooks_path = get_codex_hooks_path();
        let impersonated = test_expected_hook_specs()[0].1.clone();
        let response = hooks_list_value(
            &hooks_path,
            vec![serde_json::json!({
                "key": "/repo/.codex/hooks.json:pre_tool_use:0:0",
                "command": impersonated,
                "source": "project",
                "sourcePath": "/repo/.codex/hooks.json",
                "enabled": true,
                "trustStatus": "untrusted",
                "currentHash": "sha256:impostor",
            })],
        );

        let entries = parse_hcom_hook_entries_from_hooks_list(&response).unwrap();
        assert_eq!(entries.len(), CODEX_HOOK_COMMANDS.len());
        assert!(
            entries.iter().all(|entry| !entry.key.contains("/repo/")),
            "a project hook copying an hcom command must not receive trust state"
        );
    }

    #[test]
    #[serial]
    fn test_hcom_commands_from_another_source_path_are_not_hcom() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        // Same commands, same user layer, but a different file: not hcom's.
        let response = hooks_list_value(Path::new("/elsewhere/hooks.json"), Vec::new());

        let error = parse_hcom_hook_entries_from_hooks_list(&response)
            .expect_err("entries from a foreign hooks file must not count as hcom's");
        assert!(error.contains("missing hcom hooks"), "{error}");
    }

    #[test]
    #[serial]
    fn test_write_hook_trust_state_refuses_foreign_key() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let hooks_path = get_codex_hooks_path();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "[features]\nhooks = true\n").unwrap();

        let entries = vec![CodexHookTrustEntry {
            key: "/repo/.codex/hooks.json:pre_tool_use:0:0".to_string(),
            command: test_expected_hook_specs()[0].1.clone(),
            current_hash: "sha256:impostor".to_string(),
        }];
        let error = write_hcom_hook_trust_state(
            &config_path,
            &hooks_path,
            &entries,
            &HashSet::new(),
            "0.131.0",
            &HashMap::new(),
        )
        .expect_err("a key outside hcom's hooks.json must be refused");
        assert!(
            error.contains("does not belong to hcom's own hooks file"),
            "{error}"
        );
        assert!(
            !std::fs::read_to_string(&config_path)
                .unwrap()
                .contains("trusted_hash"),
            "the refused entry must not have been written"
        );
    }

    #[test]
    #[serial]
    fn test_hook_state_key_ownership() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let hooks_path = get_codex_hooks_path();
        let key = format!("{}:pre_tool_use:0:0", hooks_path.display());
        assert!(hook_state_key_belongs_to_hcom_hooks_json(&key, &hooks_path));
        // Same file, but an event hcom does not install.
        assert!(!hook_state_key_belongs_to_hcom_hooks_json(
            &format!("{}:session_end:0:0", hooks_path.display()),
            &hooks_path
        ));
        // A different file entirely.
        assert!(!hook_state_key_belongs_to_hcom_hooks_json(
            "/repo/.codex/hooks.json:pre_tool_use:0:0",
            &hooks_path
        ));
        // Malformed positional suffix.
        assert!(!hook_state_key_belongs_to_hcom_hooks_json(
            &format!("{}:pre_tool_use:x:0", hooks_path.display()),
            &hooks_path
        ));
    }

    #[test]
    #[serial]
    fn test_foreign_hooks_unlocked_by_bypass_ignores_trusted_and_hcom() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let hooks_path = get_codex_hooks_path();
        let response = hooks_list_value(
            &hooks_path,
            vec![
                // Already trusted — the flag changes nothing for it.
                serde_json::json!({
                    "key": "/repo/.codex/hooks.json:stop:0:0",
                    "command": "trusted-tool",
                    "source": "project",
                    "sourcePath": "/repo/.codex/hooks.json",
                    "enabled": true,
                    "trustStatus": "trusted",
                    "currentHash": "sha256:a",
                }),
                // Disabled — the flag does not enable it.
                serde_json::json!({
                    "key": "/repo/.codex/hooks.json:stop:1:0",
                    "command": "disabled-tool",
                    "source": "project",
                    "sourcePath": "/repo/.codex/hooks.json",
                    "enabled": false,
                    "trustStatus": "untrusted",
                    "currentHash": "sha256:b",
                }),
            ],
        );
        let entries = parse_codex_hook_list_entries(&response).unwrap();
        assert!(foreign_hooks_unlocked_by_bypass(&entries, &hooks_path).is_empty());
    }

    #[test]
    #[serial]
    fn test_foreign_hooks_unlocked_by_bypass_flags_modified_and_unknown() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let hooks_path = get_codex_hooks_path();
        let response = hooks_list_value(
            &hooks_path,
            vec![
                serde_json::json!({
                    "key": "/repo/.codex/hooks.json:stop:0:0",
                    "command": "modified-tool",
                    "source": "project",
                    "sourcePath": "/repo/.codex/hooks.json",
                    "enabled": true,
                    "trustStatus": "modified",
                    "currentHash": "sha256:a",
                }),
                // No trustStatus at all: hcom cannot prove it is safe.
                serde_json::json!({
                    "key": "/plugin/hooks.json:stop:0:0",
                    "command": "plugin-tool",
                    "source": "plugin",
                    "sourcePath": "/plugin/hooks.json",
                    "enabled": true,
                    "currentHash": "sha256:b",
                }),
            ],
        );
        let entries = parse_codex_hook_list_entries(&response).unwrap();
        let foreign = foreign_hooks_unlocked_by_bypass(&entries, &hooks_path);
        assert_eq!(foreign.len(), 2, "{foreign:?}");
        assert!(foreign.iter().any(|f| f.contains("modified-tool")));
        assert!(foreign.iter().any(|f| f.contains("plugin-tool")));
    }

    #[test]
    fn test_paths_equivalent_handles_dot_components() {
        assert!(paths_equivalent(
            Path::new("/home/u/.codex/./hooks.json"),
            Path::new("/home/u/.codex/hooks.json")
        ));
        assert!(paths_equivalent(
            Path::new("/home/u/other/../.codex/hooks.json"),
            Path::new("/home/u/.codex/hooks.json")
        ));
        assert!(!paths_equivalent(
            Path::new("/home/u/.codex/hooks.json"),
            Path::new("/repo/.codex/hooks.json")
        ));
    }

    #[test]
    fn test_hcom_command_for_hook_state_key() {
        assert_eq!(
            hcom_command_for_hook_state_key("/tmp/codex/hooks.json:pre_tool_use:0:0"),
            build_codex_hook_command("codex-pretooluse")
        );
    }

    #[test]
    #[serial]
    fn test_setup_preserves_unrelated_hooks() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let hooks_path = get_codex_hooks_path();
        std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        std::fs::write(
            &hooks_path,
            serde_json::json!({
                "hooks": {
                    "PostToolUse": [{
                        "matcher": "Bash",
                        "hooks": [{"type": "command", "command": "other-hook"}]
                    }]
                }
            })
            .to_string(),
        )
        .unwrap();

        assert!(setup_codex_hooks(false));
        let content = std::fs::read_to_string(hooks_path).unwrap();
        assert!(content.contains("other-hook"));
        assert!(content.contains("codex-posttooluse"));
    }

    #[test]
    #[serial]
    fn test_mixed_group_merge_preserves_user_hooks() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let hooks_path = get_codex_hooks_path();
        std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        std::fs::write(
            &hooks_path,
            serde_json::json!({
                "hooks": {
                    "PostToolUse": [{
                        "matcher": "Bash",
                        "hooks": [
                            {"type": "command", "command": "user-mixed-hook"},
                            {"type": "command", "command": "old-path codex-posttooluse"}
                        ]
                    }]
                }
            })
            .to_string(),
        )
        .unwrap();

        assert!(setup_codex_hooks(false));
        let content = std::fs::read_to_string(&hooks_path).unwrap();
        assert!(content.contains("user-mixed-hook"), "user hook was dropped");
        assert!(content.contains("codex-posttooluse"), "hcom hook missing");
        let json: Value = serde_json::from_str(&content).unwrap();
        let posttool_groups = json["hooks"]["PostToolUse"].as_array().unwrap();
        let bash_group = posttool_groups
            .iter()
            .find(|g| g.get("matcher").and_then(|v| v.as_str()) == Some("Bash"))
            .expect("Bash group missing");
        let hook_cmds: Vec<&str> = bash_group["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|h| h.get("command").and_then(|v| v.as_str()))
            .collect();
        let hcom_count = hook_cmds
            .iter()
            .filter(|c| c.contains("codex-posttooluse"))
            .count();
        assert_eq!(
            hcom_count, 1,
            "expected exactly one hcom hook, got {hcom_count}"
        );
    }

    #[test]
    #[serial]
    fn test_mixed_group_remove_preserves_user_hooks() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let hooks_path = get_codex_hooks_path();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "[features]\nhooks = true\n").unwrap();
        std::fs::write(
            &hooks_path,
            serde_json::json!({
                "hooks": {
                    "PostToolUse": [{
                        "matcher": "Bash",
                        "hooks": [
                            {"type": "command", "command": "user-remove-hook"},
                            {"type": "command", "command": "old-path codex-posttooluse"}
                        ]
                    }]
                }
            })
            .to_string(),
        )
        .unwrap();

        assert!(remove_codex_hooks());
        assert!(
            hooks_path.exists(),
            "hooks.json was deleted but user hook was present"
        );
        let content = std::fs::read_to_string(&hooks_path).unwrap();
        assert!(
            content.contains("user-remove-hook"),
            "user hook was dropped"
        );
        assert!(
            !content.contains("codex-posttooluse"),
            "hcom hook was not removed"
        );
    }

    #[test]
    #[serial]
    fn test_ensure_feature_enabled_preserves_unrelated_notify() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "notify = \"some-other-notify-tool\"\n").unwrap();

        assert!(setup_codex_hooks(false));
        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            content.contains("some-other-notify-tool"),
            "unrelated notify was removed"
        );
        assert!(content.contains("hooks = true"), "feature flag not set");
    }

    #[test]
    #[serial]
    fn test_ensure_feature_enabled_preserves_notify_with_codex_notify_but_no_hcom_owner() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "notify = \"other-tool codex-notify\"\n").unwrap();

        assert!(setup_codex_hooks(false));
        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            content.contains("other-tool codex-notify"),
            "non-hcom notify mentioning codex-notify was removed"
        );
        assert!(content.contains("hooks = true"), "feature flag not set");
    }

    #[test]
    #[serial]
    fn test_ensure_feature_enabled_removes_hcom_notify() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "notify = \"hcom internal codex-notify --name luna\"\n",
        )
        .unwrap();

        assert!(setup_codex_hooks(false));
        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            !content.contains("notify"),
            "hcom notify key was not removed"
        );
        assert!(content.contains("hooks = true"), "feature flag not set");
    }

    #[test]
    #[serial]
    fn test_remove_codex_hooks_preserves_feature_flag() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        assert!(setup_codex_hooks(false));

        let config_path = get_codex_config_path();
        let before = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            before.contains("hooks = true"),
            "setup did not enable feature flag"
        );

        assert!(remove_codex_hooks());
        let after = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            after.contains("hooks = true"),
            "feature flag should be preserved"
        );
    }

    #[test]
    #[serial]
    fn test_setup_codex_creates_execpolicy() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        assert!(setup_codex_hooks(true));

        let rules_file = get_codex_rules_path().join("hcom.rules");
        assert!(rules_file.exists(), "execpolicy rules should be created");
        let content = std::fs::read_to_string(&rules_file).unwrap();
        assert!(content.contains("hcom"));
    }

    #[test]
    #[serial]
    fn test_remove_codex_removes_execpolicy() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        assert!(setup_codex_hooks(true));
        let rules_file = get_codex_rules_path().join("hcom.rules");
        assert!(rules_file.exists());

        assert!(remove_codex_hooks());
        assert!(!rules_file.exists(), "execpolicy rules should be removed");
    }

    #[test]
    #[serial]
    fn test_remove_codex_noop_when_no_hooks_json() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        assert!(remove_codex_hooks());
    }

    #[test]
    #[serial]
    fn test_codex_feature_enabled_with_fallback() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "[features]\ncodex_hooks = true\n").unwrap();

        // Both keys resolve because the selected key is checked first,
        // then the alternate acts as a fallback.
        assert!(codex_feature_enabled(
            &config_path,
            CodexHooksFeatureKey::CodexHooks
        ));
        assert!(codex_feature_enabled(
            &config_path,
            CodexHooksFeatureKey::Hooks
        ));

        // Reverse: only hooks key present.
        std::fs::write(&config_path, "[features]\nhooks = true\n").unwrap();
        assert!(codex_feature_enabled(
            &config_path,
            CodexHooksFeatureKey::Hooks
        ));
        assert!(codex_feature_enabled(
            &config_path,
            CodexHooksFeatureKey::CodexHooks
        ));
    }

    #[test]
    #[serial]
    fn test_ensure_feature_upgrade_cleans_stale_codex_hooks() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        // Seed config with the deprecated key, simulating an old hcom install.
        std::fs::write(&config_path, "[features]\ncodex_hooks = true\n").unwrap();

        ensure_codex_feature_enabled(&config_path, CodexHooksFeatureKey::Hooks).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("hooks = true"), "upgrade should set hooks");
        assert!(
            !content.contains("codex_hooks"),
            "upgrade should remove stale codex_hooks"
        );
    }

    #[test]
    #[serial]
    fn test_ensure_feature_upgrade_cleans_profile_stale_codex_hooks() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "profile = \"work\"\n\n[features]\nhooks = true\n\n[profiles.work.features]\ncodex_hooks = true\n",
        )
        .unwrap();

        assert!(!codex_current_feature_enabled());

        ensure_codex_feature_enabled(&config_path, CodexHooksFeatureKey::Hooks).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("hooks = true"), "upgrade should set hooks");
        assert!(
            !content.contains("codex_hooks"),
            "upgrade should remove stale profile codex_hooks"
        );
        assert!(codex_current_feature_enabled());
    }

    #[test]
    #[serial]
    fn test_ensure_feature_upgrade_cleans_inline_profile_stale_codex_hooks() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "profile = \"work\"\nprofiles = { work = { features = { codex_hooks = true } } }\n\n[features]\nhooks = true\n",
        )
        .unwrap();

        assert!(!codex_current_feature_enabled());

        ensure_codex_feature_enabled(&config_path, CodexHooksFeatureKey::Hooks).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("hooks = true"), "upgrade should set hooks");
        assert!(
            !content.contains("codex_hooks"),
            "upgrade should remove stale inline profile codex_hooks"
        );
        assert!(codex_current_feature_enabled());
    }

    #[test]
    #[serial]
    fn test_current_feature_enabled_requires_selected_key() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "[features]\ncodex_hooks = true\n").unwrap();

        assert!(codex_feature_enabled(
            &config_path,
            CodexHooksFeatureKey::Hooks
        ));
        assert!(!codex_current_feature_enabled());

        ensure_codex_feature_enabled(&config_path, CodexHooksFeatureKey::Hooks).unwrap();
        assert!(codex_current_feature_enabled());
    }

    #[test]
    #[serial]
    fn test_current_feature_enabled_rejects_mixed_deprecated_key() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "[features]\nhooks = true\ncodex_hooks = true\n",
        )
        .unwrap();

        assert!(codex_feature_enabled(
            &config_path,
            CodexHooksFeatureKey::Hooks
        ));
        assert!(!codex_current_feature_enabled());

        ensure_codex_feature_enabled(&config_path, CodexHooksFeatureKey::Hooks).unwrap();
        assert!(codex_current_feature_enabled());
    }

    #[test]
    #[serial]
    fn test_current_feature_enabled_rejects_profile_deprecated_key() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "profile = \"work\"\n\n[features]\nhooks = true\n\n[profiles.work.features]\ncodex_hooks = true\n",
        )
        .unwrap();

        assert!(codex_feature_enabled(
            &config_path,
            CodexHooksFeatureKey::Hooks
        ));
        assert!(!codex_current_feature_enabled());
    }

    #[test]
    #[serial]
    fn test_current_feature_enabled_ignores_inactive_profile_deprecated_key() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "[features]\nhooks = true\n\n[profiles.work.features]\ncodex_hooks = true\n",
        )
        .unwrap();

        assert!(codex_current_feature_enabled());
    }

    #[test]
    #[serial]
    fn test_current_feature_enabled_rejects_inline_profile_deprecated_key() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            "profile = \"work\"\nprofiles = { work = { features = { codex_hooks = true } } }\n\n[features]\nhooks = true\n",
        )
        .unwrap();

        assert!(codex_feature_enabled(
            &config_path,
            CodexHooksFeatureKey::Hooks
        ));
        assert!(!codex_current_feature_enabled());
    }

    #[test]
    #[serial]
    fn test_ensure_feature_downgrade_uses_codex_hooks() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let config_path = get_codex_config_path();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(&config_path, "[features]\nhooks = true\n").unwrap();

        ensure_codex_feature_enabled(&config_path, CodexHooksFeatureKey::CodexHooks).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let doc = content.parse::<DocumentMut>().unwrap();
        let features = doc.get("features").unwrap();
        assert!(
            features.get("codex_hooks").and_then(|v| v.as_bool()) == Some(true),
            "old Codex should use codex_hooks"
        );
        // hooks is the shared flag for all Codex hooks — not just hcom's.
        // hcom must not delete it even when writing for an older Codex.
        assert!(
            features.get("hooks").and_then(|v| v.as_bool()) == Some(true),
            "shared hooks flag should be preserved"
        );
    }

    #[test]
    fn test_codex_hooks_feature_key_version_gate() {
        assert_eq!(
            codex_hooks_feature_key_for_version((0, 128, 0)),
            CodexHooksFeatureKey::CodexHooks
        );
        assert_eq!(
            codex_hooks_feature_key_for_version((0, 129, 0)),
            CodexHooksFeatureKey::Hooks
        );
        assert_eq!(
            parse_codex_cli_version("codex-cli 0.129.0"),
            Some((0, 129, 0))
        );
    }

    // ── regression: legacy "cmd"-format cleanup ─────────────────────────────

    /// Old hcom versions wrote hooks as {"type":"cmd","cmd":"hcom codex-..."}.
    /// On Codex >= 0.129 (CODEX_HOOKS_FEATURE_RENAME_VERSION), try_setup_codex_hooks
    /// must remove those stale entries and replace them with the current format.
    ///
    /// FAILS before the fix: remove_legacy_hcom_cmd_hooks_from_json is defined
    /// but not yet called from try_setup_codex_hooks.
    #[test]
    #[serial]
    fn test_legacy_cmd_hooks_cleaned_on_new_codex() {
        // isolated_test_env sets HCOM_TEST_CODEX_CLI_VERSION = "codex-cli 0.129.0"
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let hooks_path = get_codex_hooks_path();
        std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        std::fs::write(
            &hooks_path,
            serde_json::json!({
                "hooks": {
                    "UserPromptSubmit": [{
                        "hooks": [{"type": "cmd", "cmd": "hcom codex-userpromptsubmit"}]
                    }]
                }
            })
            .to_string(),
        )
        .unwrap();

        assert!(try_setup_codex_hooks(false).is_ok());
        let content = std::fs::read_to_string(&hooks_path).unwrap();
        let hooks_json: Value = serde_json::from_str(&content).unwrap();
        let user_prompt_hooks = hooks_json["hooks"]["UserPromptSubmit"][0]["hooks"]
            .as_array()
            .unwrap();
        assert!(
            !user_prompt_hooks
                .iter()
                .any(|hook| hook["type"] == "cmd" && hook.get("cmd").is_some()),
            "legacy cmd-keyed entry must be removed on Codex >= 0.129"
        );
        assert!(
            user_prompt_hooks.iter().any(|hook| {
                hook["type"] == "command"
                    && hook["command"] == build_codex_hook_command("codex-userpromptsubmit")
            }),
            "current command-keyed entry must be present after cleanup"
        );
    }

    // ── regression: context-mode "matcher":"" groups must not block verify ──

    /// context-mode writes groups with "matcher":"" for None-matcher events
    /// (UserPromptSubmit, Stop).  Those groups appear before hcom's no-matcher
    /// groups in the file.  verify_hooks_json_value must not pick the wrong
    /// group and report HookCommandMissing.
    #[test]
    #[serial]
    fn test_context_mode_empty_matcher_does_not_block_verify() {
        let (_tmp, _hcom_dir, _home, _guard) = isolated_test_env();
        let hooks_path = get_codex_hooks_path();
        std::fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
        // Seed with context-mode "matcher":"" groups appearing FIRST for both
        // None-matcher events, matching the live ~/.codex/hooks.json layout.
        std::fs::write(
            &hooks_path,
            serde_json::json!({
                "hooks": {
                    "UserPromptSubmit": [{
                        "matcher": "",
                        "hooks": [{"type": "command", "command": "context-mode hook codex userpromptsubmit"}]
                    }],
                    "Stop": [{
                        "matcher": "",
                        "hooks": [{"type": "command", "command": "context-mode hook codex stop"}]
                    }]
                }
            })
            .to_string(),
        )
        .unwrap();

        assert!(
            try_setup_codex_hooks(false).is_ok(),
            "setup must succeed even when another tool owns a \"matcher\":\"\" group for the same event"
        );
        let content = std::fs::read_to_string(&hooks_path).unwrap();
        assert!(
            content.contains("context-mode hook codex userpromptsubmit"),
            "third-party hook must be preserved"
        );
        assert!(
            content.contains("codex-userpromptsubmit"),
            "hcom hook must be present"
        );
    }

    #[test]
    #[serial]
    fn remove_codex_hooks_cleans_active_hcom_dir_local_path() {
        let _guard = EnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let workspace = dir.path().join("workspace");
        let local_dir = workspace.join(".codex");
        std::fs::create_dir_all(local_dir.join("rules")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("HCOM_DIR", workspace.join(".hcom"));
            std::env::remove_var("CODEX_HOME");
        }
        std::fs::write(
            local_dir.join("hooks.json"),
            serde_json::to_string_pretty(&build_expected_hook_json()).unwrap(),
        )
        .unwrap();
        std::fs::write(local_dir.join("rules/hcom.rules"), "allow").unwrap();

        assert!(remove_codex_hooks());
        assert!(!local_dir.join("rules/hcom.rules").exists());
        if local_dir.join("hooks.json").exists() {
            let content = std::fs::read_to_string(local_dir.join("hooks.json")).unwrap();
            assert!(!content.contains("codex-"));
        }
    }
}
