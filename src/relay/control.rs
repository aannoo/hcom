//! Control events — remote RPC over the MQTT control topic.
//!
//! Control messages are published to `{relay_id}/control` (non-retained) and
//! currently carry request/response style remote actions.

use rumqttc::v5::mqttbytes::QoS;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

use crate::config::HcomConfig;
use crate::db::HcomDb;
use crate::launcher::{self, LaunchParams};
use crate::log;

use super::{
    control_topic, crypto, device_short_id, is_relay_enabled, load_psk, read_device_uuid,
    safe_kv_get, safe_kv_set,
};

/// Build a sealed RPC control envelope ready to publish. Returns the topic and
/// the AEAD-sealed bytes; the underlying JSON layout is unchanged from the
/// pre-encryption format so callers don't have to know about the cipher.
fn build_control_payload(
    config: &HcomConfig,
    action: &str,
    target_device_short_id: &str,
    request_id: Option<&str>,
    params: &serde_json::Value,
) -> Option<(String, Vec<u8>)> {
    if !is_relay_enabled(config) {
        return None;
    }

    let relay_id = &config.relay_id;
    if relay_id.is_empty() {
        return None;
    }

    let psk = match load_psk(config) {
        Ok(p) => p,
        Err(e) => {
            crate::log::log_warn("relay", "relay.psk_missing", &e);
            return None;
        }
    };

    let device_id = read_device_uuid()?;
    let short_id = device_short_id(&device_id);
    let now = crate::shared::time::now_epoch_f64();
    let mut control_data = json!({
        "action": action,
        "target_device": target_device_short_id,
        "from": format!("_:{}", short_id),
        "from_device": device_id,
        "params": params,
    });
    if let Some(request_id) = request_id {
        control_data["request_id"] = Value::String(request_id.to_string());
    }

    let control_payload = json!({
        "from_device": device_id,
        "events": [{
            "ts": now,
            "type": "control",
            "instance": "_control",
            "data": control_data,
        }],
    });

    let topic = control_topic(relay_id);
    let plaintext = serde_json::to_vec(&control_payload).ok()?;
    let sealed = crypto::seal(&psk, relay_id, &topic, &plaintext, now as u64).ok()?;
    Some((topic, sealed))
}

pub fn build_rpc_control_payload(
    config: &HcomConfig,
    action: &str,
    target_device_short_id: &str,
    request_id: &str,
    params: &serde_json::Value,
) -> Option<(String, Vec<u8>)> {
    build_control_payload(
        config,
        action,
        target_device_short_id,
        Some(request_id),
        params,
    )
}

fn send_control_via_ephemeral(
    config: &HcomConfig,
    client: &super::client::EphemeralClient,
    action: &str,
    target_device_short_id: &str,
    request_id: Option<&str>,
    params: &serde_json::Value,
) -> bool {
    let (topic, payload_bytes) =
        match build_control_payload(config, action, target_device_short_id, request_id, params) {
            Some(v) => v,
            None => return false,
        };

    let result = client.publish_and_wait(
        &topic,
        QoS::AtLeastOnce,
        false,
        payload_bytes,
        Duration::from_secs(5),
    );

    if result {
        if let Some(request_id) = request_id {
            log::log_with_fields(
                "INFO",
                "relay",
                "relay.control",
                "",
                &[
                    ("action", action),
                    ("target", target_device_short_id),
                    ("request_id", request_id),
                ],
            );
        } else {
            log::log_with_fields(
                "INFO",
                "relay",
                "relay.control",
                "",
                &[("action", action), ("target", target_device_short_id)],
            );
        }
    } else {
        log::log_warn("relay", "relay.network", "control: PUBACK timeout");
    }

    result
}

/// Send an RPC control command using an ephemeral client.
pub fn send_rpc_control_ephemeral(
    config: &HcomConfig,
    action: &str,
    target_device_short_id: &str,
    request_id: &str,
    params: &serde_json::Value,
) -> bool {
    let ephemeral = match super::client::create_ephemeral_client(config) {
        Some(c) => c,
        None => return false,
    };

    let result = send_control_via_ephemeral(
        config,
        &ephemeral,
        action,
        target_device_short_id,
        Some(request_id),
        params,
    );

    ephemeral.disconnect();
    result
}

pub fn send_one_way_control_ephemeral(
    config: &HcomConfig,
    action: &str,
    target_device_short_id: &str,
    params: &serde_json::Value,
) -> bool {
    let ephemeral = match super::client::create_ephemeral_client(config) {
        Some(c) => c,
        None => return false,
    };

    let result = send_control_via_ephemeral(
        config,
        &ephemeral,
        action,
        target_device_short_id,
        None,
        params,
    );

    ephemeral.disconnect();
    result
}

pub fn get_rpc_result(db: &HcomDb, request_id: &str) -> Option<Value> {
    let data: String = db
        .conn()
        .query_row(
            "SELECT data FROM events
             WHERE type = 'rpc_result'
             AND json_extract(data, '$.request_id') = ?
             ORDER BY id DESC LIMIT 1",
            rusqlite::params![request_id],
            |row| row.get(0),
        )
        .ok()?;
    serde_json::from_str(&data).ok()
}

fn delete_rpc_result(db: &HcomDb, request_id: &str) {
    // rpc_result rows are single-use rendezvous points. Deleting on consume prevents
    // accumulation and keeps _rpc events out of the push loop entirely.
    let _ = db.conn().execute(
        "DELETE FROM events WHERE type = 'rpc_result' AND json_extract(data, '$.request_id') = ?",
        rusqlite::params![request_id],
    );
}

pub fn wait_for_rpc_result_with_db(
    db: &HcomDb,
    request_id: &str,
    timeout: Duration,
) -> Result<Value, String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(result) = get_rpc_result(db, request_id) {
            delete_rpc_result(db, request_id);
            return Ok(result);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("timed out waiting for rpc_result {}", request_id))
}

/// Send an RPC request and wait for the result, reusing an existing HcomDb connection.
/// Prefer this over `send_rpc_request_and_wait` when the caller already holds a db.
pub fn send_rpc_request_and_wait_with_db(
    db: &HcomDb,
    config: &HcomConfig,
    action: &str,
    target_device_short_id: &str,
    target_name: Option<&str>,
    params: &serde_json::Value,
    timeout: Duration,
) -> Result<Value, String> {
    if !super::worker::ensure_worker(false) {
        return Err("relay worker not running - start with: hcom relay on".to_string());
    }
    ensure_remote_action_supported(db, target_device_short_id, action, target_name)?;
    let request_id = uuid::Uuid::new_v4().to_string();
    if !send_rpc_control_ephemeral(config, action, target_device_short_id, &request_id, params) {
        return Err(format!("failed to send {} request", action));
    }
    wait_for_rpc_result_with_db(db, &request_id, timeout)
}

pub fn require_successful_rpc_result(response: Value) -> Result<Value, String> {
    if response
        .get("ok")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Ok(response);
    }

    let action = response
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("remote command");
    let result = response.get("result").unwrap_or(&Value::Null);
    let detail = result
        .get("error")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let rendered = serde_json::to_string(result).unwrap_or_default();
            if rendered.is_empty() || rendered == "null" {
                "unknown remote error".to_string()
            } else {
                rendered
            }
        });
    Err(format!("{action} failed: {detail}"))
}

/// Send an RPC request and return the raw response envelope
/// (`{ok, action, result}`). Loads config internally.
///
/// Prefer [`dispatch_remote`] unless the caller needs to inspect custom
/// fields on a failed response (e.g. kill's `permission_denied` path).
pub fn dispatch_remote_raw(
    db: &HcomDb,
    device_short_id: &str,
    target_name: Option<&str>,
    action: &str,
    params: &Value,
    timeout: Duration,
) -> Result<Value, String> {
    let config = HcomConfig::load(None).unwrap_or_default();
    send_rpc_request_and_wait_with_db(
        db,
        &config,
        action,
        device_short_id,
        target_name,
        params,
        timeout,
    )
}

/// Send an RPC request, require success, and return the unwrapped inner
/// `result` field. This is the default for CLI callers: on success you get
/// the handler's output value; on failure you get a user-facing error string.
pub fn dispatch_remote(
    db: &HcomDb,
    device_short_id: &str,
    target_name: Option<&str>,
    action: &str,
    params: &Value,
    timeout: Duration,
) -> Result<Value, String> {
    let response = dispatch_remote_raw(db, device_short_id, target_name, action, params, timeout)?;
    let response = require_successful_rpc_result(response)?;
    Ok(response.get("result").cloned().unwrap_or(Value::Null))
}

/// Dispatch a remote RPC and print the result field to stdout.
/// Returns the CLI exit code (0 on success, 1 on error).
/// `result_key` is the JSON key in the result to extract (e.g. "content", "message").
#[allow(clippy::too_many_arguments)]
pub fn dispatch_remote_and_print(
    db: &HcomDb,
    device_short_id: &str,
    target_name: Option<&str>,
    action: &str,
    params: &Value,
    timeout: Duration,
    result_key: &str,
    fallback_msg: &str,
) -> i32 {
    match dispatch_remote(db, device_short_id, target_name, action, params, timeout) {
        Ok(inner) => {
            println!("{}", inner[result_key].as_str().unwrap_or(fallback_msg));
            0
        }
        Err(e) => {
            eprintln!("Remote {action} failed: {e}");
            1
        }
    }
}

/// Default timeout for remote RPC commands (kill, resume, term, transcript, config).
pub const RPC_DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Longer timeout for remote launch, which may need to pull a model and start a process.
pub const RPC_LAUNCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Remote resume waits on a real relaunch on the target device, so it can
/// legitimately take longer than the generic term/config/kill RPC budget.
pub const RPC_RESUME_TIMEOUT: Duration = Duration::from_secs(30);

/// Split a target name like "luna:ABCD" into ("luna", "ABCD").
/// Only returns Some when the suffix is exactly 4 uppercase alphanumeric characters —
/// the format used for device short IDs. This prevents plain colons in agent names
/// (e.g. "tag:value") from being misidentified as remote targets.
pub fn split_device_suffix(name: &str) -> Option<(&str, &str)> {
    let (base, suffix) = name.rsplit_once(':')?;
    if suffix.len() == 4
        && suffix
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        Some((base, suffix))
    } else {
        None
    }
}

/// RPC action name constants — use these instead of raw string literals.
pub mod rpc_action {
    pub const LAUNCH: &str = "launch";
    pub const KILL: &str = "kill";
    pub const RESUME: &str = "resume";
    pub const CONFIG_GET: &str = "config_get";
    pub const CONFIG_SET: &str = "config_set";
    pub const RELAY_OFF: &str = "relay_off";
    pub const TERM_SCREEN: &str = "term_screen";
    pub const TERM_INJECT: &str = "term_inject";
    pub const TRANSCRIPT: &str = "transcript";
    pub const EVENTS: &str = "events";
    pub const SUB_CREATE: &str = "sub_create";
    pub const SUB_LIST: &str = "sub_list";
    pub const SUB_UNSUB: &str = "sub_unsub";
}

type RemoteRpcHandler = fn(&HcomDb, &Value, &str, &HcomConfig) -> Result<Value, String>;

const REMOTE_RPC_HANDLERS: &[(&str, RemoteRpcHandler)] = &[
    (rpc_action::LAUNCH, handle_remote_launch),
    (rpc_action::KILL, handle_remote_kill),
    (rpc_action::RESUME, handle_remote_resume),
    (rpc_action::CONFIG_GET, handle_remote_config_get),
    (rpc_action::CONFIG_SET, handle_remote_config_set),
    (rpc_action::RELAY_OFF, handle_remote_relay_off),
    (rpc_action::TERM_SCREEN, handle_remote_term_screen),
    (rpc_action::TERM_INJECT, handle_remote_term_inject),
    (rpc_action::TRANSCRIPT, handle_remote_transcript),
    (rpc_action::EVENTS, handle_remote_events),
    (rpc_action::SUB_CREATE, handle_remote_sub_create),
    (rpc_action::SUB_LIST, handle_remote_sub_list),
    (rpc_action::SUB_UNSUB, handle_remote_sub_unsub),
];

pub fn advertised_remote_capabilities() -> Vec<&'static str> {
    REMOTE_RPC_HANDLERS
        .iter()
        .map(|(action, _)| *action)
        .collect()
}

fn find_remote_rpc_handler(action: &str) -> Option<RemoteRpcHandler> {
    REMOTE_RPC_HANDLERS
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(_, handler)| *handler)
}

fn allows_one_way_remote_action(action: &str) -> bool {
    action == rpc_action::RELAY_OFF
}

/// Peers whose last relay sync is older than this are considered offline.
/// Must match REMOTE_DEVICE_STALE_THRESHOLD in instance_lifecycle.rs.
const PEER_STALE_THRESHOLD_SECS: f64 = 90.0;

/// Three distinct states a remote peer's capability advertisement can be in.
/// Keeping these separate is load-bearing for mixed-version relays:
/// a peer that predates the capability field (`Legacy`) must NOT be
/// hard-blocked like one that explicitly advertises `[]`.
#[derive(Debug, PartialEq)]
enum CachedCapabilities {
    /// No state message from this peer yet — caller should retry briefly.
    NotSynced,
    /// Peer was seen before but its last sync is older than the staleness
    /// threshold. Contains the age in whole seconds for error messages.
    Stale(u64),
    /// State arrived but had no `capabilities` field. Pre-capability peer;
    /// let the request through and rely on the RPC timeout if unsupported.
    Legacy,
    /// Peer explicitly advertised a (possibly empty) capability list.
    Advertised(Vec<String>),
}

fn read_remote_capabilities(
    db: &HcomDb,
    target_device_short_id: &str,
) -> Result<CachedCapabilities, String> {
    let Some(device_id) = safe_kv_get(db, &format!("relay_short_{}", target_device_short_id))
    else {
        return Ok(CachedCapabilities::NotSynced);
    };

    // Check freshness before trusting cached capabilities.
    let sync_time = safe_kv_get(db, &format!("relay_sync_time_{}", device_id))
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    if sync_time > 0.0 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let age = now - sync_time;
        if age > PEER_STALE_THRESHOLD_SECS {
            return Ok(CachedCapabilities::Stale(age as u64));
        }
    }

    let Some(raw) = safe_kv_get(db, &format!("relay_caps_{}", device_id)) else {
        return Ok(CachedCapabilities::NotSynced);
    };
    if raw == "null" {
        return Ok(CachedCapabilities::Legacy);
    }
    let parsed = serde_json::from_str::<Vec<String>>(&raw).map_err(|e| {
        format!("failed to parse remote capabilities for {target_device_short_id}: {e}")
    })?;
    Ok(CachedCapabilities::Advertised(parsed))
}

fn check_remote_action_for_db(
    db: &HcomDb,
    target_device_short_id: &str,
    action: &str,
    target_name: Option<&str>,
) -> Result<(), String> {
    let detail = target_name
        .map(|n| format!(" (target {})", n))
        .unwrap_or_default();
    match read_remote_capabilities(db, target_device_short_id)? {
        CachedCapabilities::Advertised(capabilities) => {
            if capabilities.iter().any(|cap| cap == action) {
                Ok(())
            } else {
                Err(format!(
                    "device {target_device_short_id}{detail} does not advertise remote action '{action}' — peer may be running an older binary, or its relay worker may need a restart to pick up newly-installed capabilities (hcom relay off && hcom relay on on the peer)."
                ))
            }
        }
        // Pre-capability peer: forward the request optimistically. If the
        // peer can't handle it, the RPC wait will time out and surface a
        // clear "timed out waiting for rpc_result" error.
        CachedCapabilities::Legacy => Ok(()),
        CachedCapabilities::Stale(age_secs) => {
            let age_str = crate::shared::time::format_age(age_secs as i64);
            Err(format!(
                "device {target_device_short_id}{detail} is offline (last seen {age_str} ago) — is the remote relay worker running?"
            ))
        }
        CachedCapabilities::NotSynced => Err(format!(
            "device {target_device_short_id}{detail} has not yet synced remote capabilities — try again in a few seconds"
        )),
    }
}

fn ensure_remote_action_supported(
    db: &HcomDb,
    target_device_short_id: &str,
    action: &str,
    target_name: Option<&str>,
) -> Result<(), String> {
    const RETRY_DELAY: Duration = Duration::from_millis(500);

    for attempt in 0..3u32 {
        if attempt > 0 {
            std::thread::sleep(RETRY_DELAY);
        }
        match read_remote_capabilities(db, target_device_short_id)? {
            // Cache is warm or peer is stale — resolve immediately, no retry.
            CachedCapabilities::Advertised(_)
            | CachedCapabilities::Legacy
            | CachedCapabilities::Stale(_) => {
                return check_remote_action_for_db(db, target_device_short_id, action, target_name);
            }
            CachedCapabilities::NotSynced => {
                // Capabilities not yet synced; keep retrying
                if attempt < 2 {
                    continue;
                }
                return check_remote_action_for_db(db, target_device_short_id, action, target_name);
            }
        }
    }
    unreachable!()
}

fn emit_rpc_result(
    db: &HcomDb,
    request_id: &str,
    action: &str,
    ok: bool,
    result: &serde_json::Value,
) {
    let data = json!({
        "request_id": request_id,
        "action": action,
        "ok": ok,
        "result": result,
    });
    let _ = db.log_event("rpc_result", "_rpc", &data);
}

fn resolve_remote_cwd(requested: Option<&str>) -> Result<String, String> {
    let requested = requested.unwrap_or("");
    if requested.is_empty() {
        return Err(
            "remote launch requires a working directory (--dir) but none was provided".to_string(),
        );
    }
    if std::path::Path::new(requested).is_dir() {
        return Ok(requested.to_string());
    }
    Err(format!(
        "requested cwd does not exist or is not a directory: {}",
        requested
    ))
}

fn required_param<'a>(params: &'a Value, key: &str) -> Result<&'a str, String> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("missing {key}"))
}

fn optional_param<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(|v| v.as_str())
}

fn bool_param(params: &Value, key: &str, default: bool) -> bool {
    params.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
}

fn usize_param(params: &Value, key: &str, default: usize) -> usize {
    params
        .get(key)
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(default)
}

fn string_list_param(params: &Value, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect()
}

fn normalize_config_field(field: &str) -> String {
    if field.starts_with("HCOM_") {
        field.to_string()
    } else {
        format!("HCOM_{}", field.to_uppercase())
    }
}

struct RemoteLaunchRequest {
    tool: String,
    count: usize,
    args: Vec<String>,
    tag: Option<String>,
    launcher: Option<String>,
    system_prompt: Option<String>,
    initial_prompt: Option<String>,
    background: bool,
    pty: bool,
    terminal: Option<String>,
    cwd: Option<String>,
}

impl RemoteLaunchRequest {
    fn from_params(params: &Value) -> Result<Self, String> {
        let count = usize_param(params, "count", 1);
        if count == 0 {
            return Err("count must be at least 1".to_string());
        }
        Ok(Self {
            tool: required_param(params, "tool")?.to_string(),
            count,
            args: string_list_param(params, "args"),
            tag: optional_param(params, "tag").map(ToString::to_string),
            launcher: optional_param(params, "launcher").map(ToString::to_string),
            system_prompt: optional_param(params, "system_prompt").map(ToString::to_string),
            initial_prompt: optional_param(params, "initial_prompt").map(ToString::to_string),
            background: bool_param(params, "background", false),
            pty: bool_param(params, "pty", false),
            terminal: optional_param(params, "terminal").map(ToString::to_string),
            cwd: optional_param(params, "cwd").map(ToString::to_string),
        })
    }
}

struct PreparedRemoteLaunch {
    args: Vec<String>,
    background: bool,
    pty: bool,
}

fn prepare_remote_launch(
    request: &RemoteLaunchRequest,
    config: &HcomConfig,
) -> PreparedRemoteLaunch {
    let (args, background, pty) = crate::commands::launch::prepare_launch_execution(
        &request.tool,
        &request.args,
        config,
        request.background,
        request.pty,
        request.initial_prompt.as_deref(),
    );
    PreparedRemoteLaunch {
        args,
        background,
        pty,
    }
}

struct RemoteResumeRequest {
    target: String,
    fork: bool,
    extra_args: Vec<String>,
    launcher: Option<String>,
}

impl RemoteResumeRequest {
    fn from_params(params: &Value) -> Result<Self, String> {
        Ok(Self {
            target: required_param(params, "target")?.to_string(),
            fork: bool_param(params, "fork", false),
            extra_args: string_list_param(params, "extra_args"),
            launcher: optional_param(params, "launcher").map(ToString::to_string),
        })
    }
}

fn handle_remote_launch(
    db: &HcomDb,
    params: &Value,
    _initiated_by: &str,
    config: &HcomConfig,
) -> Result<Value, String> {
    let request = RemoteLaunchRequest::from_params(params)?;
    let prepared = prepare_remote_launch(&request, config);
    // Enforce the same Claude headless invariant the local CLI path enforces
    // (src/commands/launch.rs validate_claude_headless_launch). The local
    // path short-circuits into dispatch_remote before local validation fires,
    // so a bare `hcom claude --headless --device X` would otherwise bypass it
    // and fall through to a detached plain-claude launch on the remote.
    crate::commands::launch::validate_claude_headless_launch(
        &request.tool,
        prepared.background,
        prepared.pty,
        &prepared.args,
        request.initial_prompt.as_deref(),
    )
    .map_err(|e| e.to_string())?;
    let cwd = resolve_remote_cwd(request.cwd.as_deref())?;

    let result = launcher::launch(
        db,
        LaunchParams {
            tool: request.tool.clone(),
            count: request.count,
            args: prepared.args,
            tag: request.tag,
            system_prompt: request.system_prompt,
            initial_prompt: request.initial_prompt,
            pty: prepared.pty,
            background: prepared.background,
            cwd: Some(cwd.clone()),
            env: None,
            launcher: request.launcher.or_else(|| Some("user".to_string())),
            // Remote launch runs inside the relay worker, so current-terminal exec
            // is not safe even for single interactive launches.
            run_here: Some(false),
            batch_id: None,
            name: None,
            skip_validation: false,
            terminal: request.terminal,
            append_reply_handoff: false,
        },
    )
    .map_err(|e| e.to_string())?;

    Ok(crate::commands::launch::launch_result_to_json(&result))
}

fn handle_remote_kill(
    db: &HcomDb,
    params: &Value,
    initiated_by: &str,
    _config: &HcomConfig,
) -> Result<Value, String> {
    let target = required_param(params, "target")?;
    let result = crate::commands::kill::kill_tracked_instance(db, target, initiated_by)?;
    Ok(json!({
        "target": result.target,
        "pid": result.pid,
        "kill_result": match result.kill_result {
            crate::terminal::KillResult::Sent => "sent",
            crate::terminal::KillResult::AlreadyDead => "already_dead",
            crate::terminal::KillResult::PermissionDenied => "permission_denied",
        },
        "pane_closed": result.pane_closed,
        "preset_name": result.preset_name,
        "pane_id": result.pane_id,
        "ok": !matches!(result.kill_result, crate::terminal::KillResult::PermissionDenied),
    }))
}

fn handle_remote_resume(
    db: &HcomDb,
    params: &Value,
    _initiated_by: &str,
    _config: &HcomConfig,
) -> Result<Value, String> {
    let request = RemoteResumeRequest::from_params(params)?;
    let flags = crate::router::GlobalFlags {
        name: request.launcher,
        ..Default::default()
    };
    let result = crate::commands::resume::run_local_resume_result(
        db,
        &request.target,
        request.fork,
        &request.extra_args,
        &flags,
    )
    .map_err(|e| e.to_string())?;
    Ok(crate::commands::launch::launch_result_to_json(&result))
}

fn handle_remote_config_get(
    db: &HcomDb,
    params: &Value,
    _initiated_by: &str,
    _config: &HcomConfig,
) -> Result<Value, String> {
    if let Some(instance) = optional_param(params, "instance") {
        let key = optional_param(params, "field");
        if let Some(field) = key {
            reject_remote_secret_field(field)?;
        }
        return crate::commands::config::config_instance_get(db, instance, key);
    }
    let fields = params
        .get("fields")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut values = serde_json::Map::new();
    for field in fields {
        if let Some(field_str) = field.as_str() {
            reject_remote_secret_field(field_str)?;
            let normalized = normalize_config_field(field_str);
            let (value, source) = crate::commands::config::config_get(&normalized);
            values.insert(
                field_str.to_string(),
                json!({"value": value, "source": source}),
            );
        }
    }
    Ok(Value::Object(values))
}

fn handle_remote_config_set(
    db: &HcomDb,
    params: &Value,
    _initiated_by: &str,
    _config: &HcomConfig,
) -> Result<Value, String> {
    if let Some(instance) = optional_param(params, "instance") {
        let field = required_param(params, "field")?;
        reject_remote_secret_field(field)?;
        let value = required_param(params, "value")?;
        return crate::commands::config::config_instance_set(db, instance, field, value);
    }
    let field = required_param(params, "field")?;
    reject_remote_secret_field(field)?;
    let value = required_param(params, "value")?;
    let normalized = normalize_config_field(field);
    crate::commands::config::config_set(&normalized, value)?;
    Ok(json!({"field": field, "value": value}))
}

fn reject_remote_secret_field(field: &str) -> Result<(), String> {
    let normalized = normalize_config_field(field);
    let secret = match normalized.as_str() {
        "HCOM_RELAY_PSK" => "relay_psk",
        "HCOM_RELAY_TOKEN" => "relay_token",
        "HCOM_RELAY_ID" => "relay_id",
        "HCOM_RELAY" => "relay",
        _ => return Ok(()),
    };
    Err(format!("{secret} is not remotely queryable"))
}

pub fn disable_local_relay(config: &HcomConfig, db: &HcomDb) -> Result<bool, String> {
    let cleared_remote_state = if config.relay_enabled {
        super::client::clear_retained_state(config)
    } else {
        false
    };
    crate::commands::config::config_set("relay_enabled", "false")?;
    // Wipe runtime-health KV so a stale "ok"/error/heartbeat from the previous
    // session can't leak into status / TUI / JSON after the subsystem is off.
    // Activity watermarks (relay_last_push_id etc.) are intentionally preserved
    // — see RUNTIME_HEALTH_KV_KEYS for the rationale.
    super::clear_runtime_relay_kv(db);
    Ok(cleared_remote_state)
}

fn handle_remote_relay_off(
    db: &HcomDb,
    _params: &Value,
    _initiated_by: &str,
    config: &HcomConfig,
) -> Result<Value, String> {
    let cleared_remote_state = disable_local_relay(config, db)?;
    if super::worker::is_relay_worker_running() {
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            let _ = super::worker::stop_relay_worker();
        });
    }
    Ok(json!({
        "disabled": true,
        "cleared_remote_state": cleared_remote_state,
    }))
}

fn handle_remote_term_screen(
    db: &HcomDb,
    params: &Value,
    _initiated_by: &str,
    _config: &HcomConfig,
) -> Result<Value, String> {
    let target = required_param(params, "target")?;
    let raw_json = bool_param(params, "json", false);
    let clean = bool_param(params, "clean", false);
    let content = crate::commands::term::read_instance_screen(db, target, raw_json, clean)?;
    Ok(json!({"target": target, "content": content}))
}

fn handle_remote_term_inject(
    db: &HcomDb,
    params: &Value,
    _initiated_by: &str,
    _config: &HcomConfig,
) -> Result<Value, String> {
    let target = required_param(params, "target")?;
    let text = optional_param(params, "text").unwrap_or("");
    let enter = bool_param(params, "enter", false);
    let message = crate::commands::term::inject_text_remote_result(db, target, text, enter)?;
    Ok(json!({"target": target, "message": message}))
}

fn handle_remote_transcript(
    db: &HcomDb,
    params: &Value,
    _initiated_by: &str,
    _config: &HcomConfig,
) -> Result<Value, String> {
    let target = required_param(params, "target")?;
    let last_n = usize_param(params, "last", 10);
    let range = optional_param(params, "range");
    let json_mode = bool_param(params, "json", false);
    let full_mode = bool_param(params, "full", false);
    let detailed = bool_param(params, "detailed", false);
    let content = crate::commands::transcript::render_instance_transcript_with_options_no_retry(
        db, target, range, last_n, json_mode, full_mode, detailed,
    )?;
    Ok(json!({"target": target, "content": content}))
}

const REMOTE_EVENTS_HARD_CAP: usize = 2000;
// Cap RPC response below the rumqttc client's 128 KiB accept limit
// (src/relay/client.rs), leaving room for envelope + AEAD overhead.
const REMOTE_EVENTS_BYTE_CAP: usize = 98_304;

fn handle_remote_events(
    db: &HcomDb,
    params: &Value,
    _initiated_by: &str,
    _config: &HcomConfig,
) -> Result<Value, String> {
    let filters: crate::core::filters::FilterMap = match params.get("filters") {
        Some(v) if !v.is_null() => {
            serde_json::from_value(v.clone()).map_err(|e| format!("invalid filters param: {e}"))?
        }
        _ => crate::core::filters::FilterMap::new(),
    };
    let sql = optional_param(params, "sql").map(|s| s.to_string());
    let mut last_n = usize_param(params, "last", 20);
    if last_n == 0 {
        last_n = 20;
    }
    if last_n > REMOTE_EVENTS_HARD_CAP {
        last_n = REMOTE_EVENTS_HARD_CAP;
    }

    crate::core::filters::validate_type_constraints(&filters)
        .map_err(|e| format!("filter error: {e}"))?;

    let mut where_clause = String::new();
    match crate::core::filters::build_sql_from_flags(&filters) {
        Ok(s) if !s.is_empty() => where_clause.push_str(&format!(" AND ({s})")),
        Ok(_) => {}
        Err(e) => return Err(format!("filter error: {e}")),
    }
    if let Some(ref s) = sql {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            where_clause.push_str(&format!(" AND ({trimmed})"));
        }
    }

    let query = format!(
        "SELECT id, timestamp, type, instance, data FROM events_v WHERE 1=1{where_clause} ORDER BY id DESC LIMIT {last_n}"
    );
    let mut stmt = db
        .conn()
        .prepare(&query)
        .map_err(|e| format!("sql error: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            let id: i64 = row.get(0)?;
            let ts: String = row.get(1)?;
            let etype: String = row.get(2)?;
            let instance: String = row.get(3)?;
            let data_str: String = row.get(4)?;
            Ok((id, ts, etype, instance, data_str))
        })
        .map_err(|e| format!("sql error: {e}"))?;

    let mut events: Vec<Value> = Vec::new();
    for row in rows {
        match row {
            Ok((id, ts, etype, instance, data_str)) => {
                let data: Value = serde_json::from_str(&data_str).unwrap_or(json!({}));
                events.push(json!({
                    "id": id,
                    "ts": ts,
                    "type": etype,
                    "instance": instance,
                    "data": data,
                }));
            }
            Err(_) => continue,
        }
    }

    let mut truncated = false;
    let build_envelope = |events: &Vec<Value>, truncated: bool| -> Value {
        let mut out = json!({"events": events, "count": events.len()});
        if truncated {
            out["truncated"] = json!(true);
        }
        out
    };
    let mut out = build_envelope(&events, truncated);
    let mut serialized_len = serde_json::to_string(&out).map(|s| s.len()).unwrap_or(0);
    while serialized_len > REMOTE_EVENTS_BYTE_CAP && !events.is_empty() {
        events.pop();
        truncated = true;
        out = build_envelope(&events, truncated);
        serialized_len = serde_json::to_string(&out).map(|s| s.len()).unwrap_or(0);
    }
    Ok(out)
}

fn handle_remote_sub_create(
    db: &HcomDb,
    params: &Value,
    _initiated_by: &str,
    _config: &HcomConfig,
) -> Result<Value, String> {
    let caller_input = required_param(params, "caller")?;
    let caller_is_external = bool_param(params, "caller_is_external", false);
    // External callers (e.g. bigboss via --as / -b) are used verbatim with no
    // instance lookup. Instance callers follow the local --for semantics:
    // exact match, then prefix fallback.
    let caller = if caller_is_external {
        caller_input.to_string()
    } else {
        crate::instances::resolve_display_name(db, caller_input)
            .or_else(|| {
                db.conn()
                    .query_row(
                        "SELECT name FROM instances WHERE name = ?",
                        rusqlite::params![caller_input],
                        |row| row.get::<_, String>(0),
                    )
                    .ok()
            })
            .or_else(|| {
                db.conn()
                    .query_row(
                        "SELECT name FROM instances WHERE name LIKE ? LIMIT 1",
                        rusqlite::params![format!("{caller_input}%")],
                        |row| row.get::<_, String>(0),
                    )
                    .ok()
            })
            .ok_or_else(|| format!("caller '{caller_input}' not found on this device"))?
    };

    let mut filters: crate::core::filters::FilterMap = match params.get("filters") {
        Some(v) if !v.is_null() => {
            serde_json::from_value(v.clone()).map_err(|e| format!("invalid filters param: {e}"))?
        }
        _ => crate::core::filters::FilterMap::new(),
    };
    // Resolve display/tag names against the remote (local-to-handler) instances table.
    crate::core::filters::resolve_filter_names(&mut filters, db);
    let sql_parts: Vec<String> = params
        .get("sql_parts")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let once = bool_param(params, "once", false);

    if filters.is_empty() && sql_parts.is_empty() {
        return Err("provide at least one filter or SQL WHERE clause".to_string());
    }

    let on_hit = params.get("on_hit").and_then(|v| v.as_str());
    let outcome = if filters.is_empty() {
        crate::commands::events::build_and_insert_sql_subscription(
            db, &sql_parts, &caller, once, on_hit,
        )?
    } else {
        crate::commands::events::build_and_insert_filter_subscription(
            db, &filters, &sql_parts, &caller, once, on_hit,
        )?
    };

    match outcome {
        crate::commands::events::SubCreateOutcome::Created { id, .. } => Ok(json!({
            "id": id,
            "caller": caller,
            "already_existed": false,
        })),
        crate::commands::events::SubCreateOutcome::AlreadyExists { id } => Ok(json!({
            "id": id,
            "caller": caller,
            "already_existed": true,
        })),
    }
}

fn handle_remote_sub_list(
    db: &HcomDb,
    _params: &Value,
    _initiated_by: &str,
    _config: &HcomConfig,
) -> Result<Value, String> {
    let rows: Vec<String> = db
        .conn()
        .prepare("SELECT value FROM kv WHERE key LIKE 'events_sub:%'")
        .map_err(|e| format!("sql error: {e}"))?
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| format!("sql error: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    let subs: Vec<Value> = rows
        .iter()
        .filter_map(|v| serde_json::from_str::<Value>(v).ok())
        .collect();

    Ok(json!({ "subs": subs }))
}

fn handle_remote_sub_unsub(
    db: &HcomDb,
    params: &Value,
    _initiated_by: &str,
    _config: &HcomConfig,
) -> Result<Value, String> {
    let id_raw = required_param(params, "id")?;
    let sub_id = if id_raw.starts_with("sub-") {
        id_raw.to_string()
    } else {
        format!("sub-{id_raw}")
    };
    let key = format!("events_sub:{sub_id}");

    let exists = db.kv_get(&key).ok().flatten().is_some();
    if !exists {
        return Ok(json!({ "id": sub_id, "removed": false }));
    }
    let _ = db.kv_set(&key, None);
    Ok(json!({ "id": sub_id, "removed": true }))
}

/// Process incoming control events targeting this device.
/// Deduplicates by timestamp to avoid re-processing.
pub fn handle_control_events(
    db: &HcomDb,
    events: &[Value],
    own_short_id: &str,
    source_device: &str,
) -> bool {
    let config = HcomConfig::load(None).unwrap_or_default();
    let last_ctrl_ts: f64 = safe_kv_get(db, &format!("relay_ctrl_{}", source_device))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0);

    let mut max_ctrl_ts = last_ctrl_ts;
    let mut produced_rpc_results = false;

    for event in events {
        if event.get("type").and_then(|v| v.as_str()) != Some("control") {
            continue;
        }

        let event_ts = event.get("ts").and_then(|v| v.as_f64()).unwrap_or(0.0);

        // Dedup by timestamp
        if event_ts <= last_ctrl_ts {
            continue;
        }
        max_ctrl_ts = max_ctrl_ts.max(event_ts);

        let data = match event.get("data") {
            Some(d) => d,
            None => continue,
        };

        let target_device = data
            .get("target_device")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_uppercase();

        if target_device != own_short_id.to_uppercase() {
            continue; // Not for us
        }

        let action = data.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let request_id = data
            .get("request_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let params = data.get("params").cloned().unwrap_or_else(|| {
            json!({
                "target": data.get("target").and_then(|v| v.as_str()).unwrap_or("")
            })
        });

        {
            let Some(handler) = find_remote_rpc_handler(action) else {
                continue;
            };
            if request_id.is_empty() && !allows_one_way_remote_action(action) {
                continue;
            }
            let initiated_by = data
                .get("from")
                .and_then(|v| v.as_str())
                .unwrap_or("remote");
            let rpc_result = handler(db, &params, initiated_by, &config);

            if request_id.is_empty() {
                if let Err(error) = rpc_result {
                    log::log_warn(
                        "relay",
                        "relay.control_one_way_err",
                        &format!("action={} error={}", action, error),
                    );
                }
                continue;
            }

            produced_rpc_results = true;
            match rpc_result {
                Ok(result) => {
                    let ok = result.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
                    emit_rpc_result(db, request_id, action, ok, &result);
                }
                Err(error) => {
                    emit_rpc_result(db, request_id, action, false, &json!({ "error": error }));
                }
            }
        }
    }

    // Persist dedup timestamp
    if max_ctrl_ts > last_ctrl_ts {
        safe_kv_set(
            db,
            &format!("relay_ctrl_{}", source_device),
            Some(&max_ctrl_ts.to_string()),
        );
    }

    produced_rpc_results
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_db() -> HcomDb {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();
        std::mem::forget(dir);
        db
    }

    fn latest_rpc_result(db: &HcomDb) -> serde_json::Value {
        let payload: String = db
            .conn()
            .query_row(
                "SELECT data FROM events WHERE type = 'rpc_result' ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_str(&payload).unwrap()
    }

    #[test]
    fn test_split_device_suffix_valid() {
        assert_eq!(split_device_suffix("luna:ABCD"), Some(("luna", "ABCD")));
        assert_eq!(
            split_device_suffix("myagent:XY12"),
            Some(("myagent", "XY12"))
        );
        assert_eq!(split_device_suffix("a:B3C4"), Some(("a", "B3C4")));
        // All digits
        assert_eq!(split_device_suffix("foo:1234"), Some(("foo", "1234")));
    }

    #[test]
    fn test_split_device_suffix_invalid() {
        // Lowercase in suffix
        assert_eq!(split_device_suffix("luna:abcd"), None);
        // Mixed case
        assert_eq!(split_device_suffix("luna:ABCd"), None);
        // Too short
        assert_eq!(split_device_suffix("luna:ABC"), None);
        // Too long
        assert_eq!(split_device_suffix("luna:ABCDE"), None);
        // No colon
        assert_eq!(split_device_suffix("luna"), None);
        // tag:value pattern should not match
        assert_eq!(split_device_suffix("tag:something"), None);
        // Hyphen in suffix
        assert_eq!(split_device_suffix("foo:AB-D"), None);
        // Empty base
        assert_eq!(split_device_suffix(":ABCD"), Some(("", "ABCD")));
    }

    #[test]
    fn test_handle_control_events_filters_by_target() {
        // Control events targeting a different device should be ignored
        let events = vec![json!({
            "type": "control",
            "ts": 1000.0,
            "data": {
                "action": "kill",
                "target_device": "ABCD",
                "from": "_:EFGH",
                "request_id": "req-other-device",
                "params": {
                    "target": "luna",
                }
            }
        })];

        // own_short_id is "WXYZ" — event targets "ABCD", so nothing should happen
        let db = test_db();
        assert!(!handle_control_events(&db, &events, "WXYZ", "device-123"));

        // No crash, no panic — event was filtered
    }

    #[test]
    fn test_resolve_remote_cwd_rejects_missing_requested_directory() {
        let err = resolve_remote_cwd(Some("/definitely/missing")).unwrap_err();
        assert!(err.contains("requested cwd does not exist or is not a directory"));
    }

    #[test]
    fn test_resolve_remote_cwd_rejects_empty() {
        let err = resolve_remote_cwd(None).unwrap_err();
        assert!(err.contains("--dir"));
        let err = resolve_remote_cwd(Some("")).unwrap_err();
        assert!(err.contains("--dir"));
    }

    #[test]
    fn test_build_rpc_control_payload_includes_request_id_and_params() {
        let psk = [0x55u8; 32];
        let config = HcomConfig {
            relay_id: "relay-1".to_string(),
            relay_psk: super::super::encode_psk(&psk),
            ..Default::default()
        };
        let (topic, sealed) = build_rpc_control_payload(
            &config,
            "launch",
            "WXYZ",
            "req-launch",
            &json!({"tool": "claude", "count": 1}),
        )
        .expect("payload");
        // Build path now produces a sealed envelope; opening with the same PSK
        // returns the original JSON.
        let plaintext =
            super::super::crypto::open(&psk, &config.relay_id, &topic, &sealed).expect("open");
        let parsed: Value = serde_json::from_slice(&plaintext).unwrap();
        let data = &parsed["events"][0]["data"];
        assert_eq!(data["action"], "launch");
        assert_eq!(data["target_device"], "WXYZ");
        assert_eq!(data["request_id"], "req-launch");
        assert_eq!(data["params"]["tool"], "claude");
    }

    #[test]
    fn test_remote_launch_request_from_params_defaults_optional_fields() {
        let request =
            RemoteLaunchRequest::from_params(&json!({"tool": "claude", "count": 2})).unwrap();
        assert_eq!(request.tool, "claude");
        assert_eq!(request.count, 2);
        assert!(request.args.is_empty());
        assert_eq!(request.tag, None);
        assert_eq!(request.launcher, None);
        assert_eq!(request.system_prompt, None);
        assert_eq!(request.initial_prompt, None);
        assert!(!request.background);
        assert_eq!(request.terminal, None);
        assert_eq!(request.cwd, None);
    }

    #[test]
    fn test_remote_launch_request_from_params_collects_terminal() {
        let request = RemoteLaunchRequest::from_params(&json!({
            "tool": "codex",
            "count": 1,
            "terminal": "kitty-tab",
            "launcher": "rega",
        }))
        .unwrap();
        assert_eq!(request.terminal.as_deref(), Some("kitty-tab"));
        assert_eq!(request.launcher.as_deref(), Some("rega"));
    }

    #[test]
    fn test_prepare_remote_launch_supports_interactive_pty() {
        let request = RemoteLaunchRequest::from_params(&json!({
            "tool": "codex",
            "count": 1,
            "args": ["--model", "gpt-5.4"],
            "background": false,
        }))
        .unwrap();
        let prepared = prepare_remote_launch(&request, &HcomConfig::default());
        assert!(!prepared.background);
        assert!(prepared.pty);
        assert_eq!(prepared.args, vec!["--model", "gpt-5.4"]);
    }

    #[test]
    fn test_prepare_remote_launch_keeps_background_detection() {
        let request = RemoteLaunchRequest::from_params(&json!({
            "tool": "claude",
            "count": 1,
            "args": ["-p"],
            "background": false,
        }))
        .unwrap();
        let prepared = prepare_remote_launch(&request, &HcomConfig::default());
        assert!(prepared.background);
        assert!(!prepared.pty);
    }

    #[test]
    fn test_prepare_remote_launch_claude_headless_with_hcom_prompt_injects_print() {
        // Remote claude --headless --hcom-prompt "..." must go through the same
        // print-mode normalization as the local path.
        let request = RemoteLaunchRequest::from_params(&json!({
            "tool": "claude",
            "count": 1,
            "args": [],
            "background": true,
            "initial_prompt": "say hi in hcom",
        }))
        .unwrap();
        let prepared = prepare_remote_launch(&request, &HcomConfig::default());
        assert!(prepared.background);
        assert!(!prepared.pty);
        let spec = crate::hooks::claude_args::resolve_claude_args(Some(&prepared.args), None);
        assert!(
            spec.is_background,
            "remote claude + --headless + --hcom-prompt must inject -p"
        );
        assert!(spec.has_flag(&["--output-format"], &["--output-format="]));
        assert!(spec.has_flag(&["--verbose"], &[]));
    }

    #[test]
    fn test_remote_launch_rejects_bare_claude_headless() {
        // Bare remote `claude --headless` with no prompt must be rejected, matching
        // the local CLI path's validate_claude_headless_launch invariant. Without
        // this, the remote handler would fall through to a detached plain-claude
        // launch. (We only assert the prepared state + validator result here,
        // without calling the full handle_remote_launch which needs a live DB.)
        let request = RemoteLaunchRequest::from_params(&json!({
            "tool": "claude",
            "count": 1,
            "args": [],
            "background": true,
        }))
        .unwrap();
        let prepared = prepare_remote_launch(&request, &HcomConfig::default());
        assert!(prepared.background);
        let err = crate::commands::launch::validate_claude_headless_launch(
            &request.tool,
            prepared.background,
            prepared.pty,
            &prepared.args,
            request.initial_prompt.as_deref(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("requires a prompt/task"));
    }

    #[test]
    fn test_remote_resume_request_from_params_collects_extra_args() {
        let request = RemoteResumeRequest::from_params(&json!({
            "target": "luna",
            "fork": true,
            "extra_args": ["--terminal", "kitty", "--model", "opus"],
            "launcher": "rega",
        }))
        .unwrap();
        assert_eq!(request.target, "luna");
        assert!(request.fork);
        assert_eq!(request.launcher.as_deref(), Some("rega"));
        assert_eq!(
            request.extra_args,
            vec!["--terminal", "kitty", "--model", "opus"]
        );
    }

    #[test]
    fn test_handle_control_events_kill_emits_rpc_result() {
        let db = test_db();
        let events = vec![json!({
            "type": "control",
            "ts": 1002.0,
            "data": {
                "action": "kill",
                "target_device": "WXYZ",
                "from": "_:EFGH",
                "request_id": "req-kill",
                "params": {
                    "target": "missing-agent",
                }
            }
        })];

        assert!(handle_control_events(&db, &events, "WXYZ", "device-123"));
        let result = latest_rpc_result(&db);
        assert_eq!(result["request_id"], "req-kill");
        assert_eq!(result["action"], "kill");
        assert_eq!(result["ok"], false);
    }

    #[test]
    fn test_handle_control_events_ignores_unknown_actions() {
        let db = test_db();
        let events = vec![json!({
            "type": "control",
            "ts": 1004.0,
            "data": {
                "action": "mystery",
                "target_device": "WXYZ",
                "from": "_:EFGH",
                "request_id": "req-unknown",
                "params": {},
            }
        })];

        assert!(!handle_control_events(&db, &events, "WXYZ", "device-123"));
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE type = 'rpc_result'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_handle_control_events_relay_off_disables_local_relay() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let config = HcomConfig {
            relay: "mqtts://broker.emqx.io:8883".to_string(),
            relay_id: "relay-1".to_string(),
            relay_psk: super::super::encode_psk(&[0x22; 32]),
            relay_enabled: true,
            ..Default::default()
        };
        crate::config::save_toml_config(&config, None).unwrap();

        let db = test_db();
        let loaded = HcomConfig::load(None).unwrap();
        let result = handle_remote_relay_off(&db, &json!({}), "_:EFGH", &loaded).unwrap();
        assert_eq!(result["disabled"], true);
        let updated = HcomConfig::load(None).unwrap();
        assert!(!updated.relay_enabled);
    }

    #[test]
    fn test_require_successful_rpc_result_returns_error_for_failed_response() {
        let err = require_successful_rpc_result(json!({
            "action": "resume",
            "ok": false,
            "result": { "error": "boom" }
        }))
        .unwrap_err();
        assert_eq!(err, "resume failed: boom");
    }

    #[test]
    fn test_check_remote_action_for_db_accepts_advertised_action() {
        let db = test_db();
        safe_kv_set(&db, "relay_short_WXYZ", Some("device-123"));
        safe_kv_set(
            &db,
            "relay_caps_device-123",
            Some(r#"["launch","resume","config_get"]"#),
        );

        assert!(check_remote_action_for_db(&db, "WXYZ", "resume", None).is_ok());
    }

    #[test]
    fn test_check_remote_action_for_db_rejects_unadvertised_action() {
        let db = test_db();
        safe_kv_set(&db, "relay_short_WXYZ", Some("device-123"));
        safe_kv_set(&db, "relay_caps_device-123", Some(r#"["launch"]"#));

        let err = check_remote_action_for_db(&db, "WXYZ", "resume", None).unwrap_err();
        assert!(
            err.starts_with("device WXYZ does not advertise remote action 'resume'"),
            "unexpected err: {err}"
        );
        assert!(
            err.contains("hcom relay off"),
            "missing restart hint: {err}"
        );
    }

    #[test]
    fn test_check_remote_action_for_db_rejects_unadvertised_action_with_target_name() {
        let db = test_db();
        safe_kv_set(&db, "relay_short_WXYZ", Some("device-123"));
        safe_kv_set(&db, "relay_caps_device-123", Some(r#"["launch"]"#));

        let err = check_remote_action_for_db(&db, "WXYZ", "kill", Some("luna:WXYZ")).unwrap_err();
        assert!(
            err.starts_with(
                "device WXYZ (target luna:WXYZ) does not advertise remote action 'kill'"
            ),
            "unexpected err: {err}"
        );
    }

    #[test]
    fn test_check_remote_action_for_db_rejects_missing_capabilities() {
        let db = test_db();
        safe_kv_set(&db, "relay_short_WXYZ", Some("device-123"));

        let err = check_remote_action_for_db(&db, "WXYZ", "resume", None).unwrap_err();
        assert!(
            err.contains("has not yet synced remote capabilities"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_check_remote_action_for_db_accepts_legacy_peer() {
        // Pre-capability peers (pull.rs stores "null" when the state message
        // carries no `capabilities` field) must be allowed through. Blocking
        // them here would break rolling upgrades: the user would see
        // "does not advertise remote action 'launch'" against a peer that
        // simply predates the capability vocabulary.
        let db = test_db();
        safe_kv_set(&db, "relay_short_WXYZ", Some("device-123"));
        safe_kv_set(&db, "relay_caps_device-123", Some("null"));

        assert_eq!(
            read_remote_capabilities(&db, "WXYZ").unwrap(),
            CachedCapabilities::Legacy
        );
        assert!(check_remote_action_for_db(&db, "WXYZ", "launch", None).is_ok());
        assert!(check_remote_action_for_db(&db, "WXYZ", "kill", None).is_ok());
    }

    #[test]
    fn test_check_remote_action_for_db_rejects_explicit_empty_capabilities() {
        // A modern peer that explicitly advertises an empty list should still
        // be hard-blocked. The legacy carve-out only applies when the field is
        // MISSING.
        let db = test_db();
        safe_kv_set(&db, "relay_short_WXYZ", Some("device-123"));
        safe_kv_set(&db, "relay_caps_device-123", Some("[]"));

        assert_eq!(
            read_remote_capabilities(&db, "WXYZ").unwrap(),
            CachedCapabilities::Advertised(Vec::new())
        );
        let err = check_remote_action_for_db(&db, "WXYZ", "launch", None).unwrap_err();
        assert!(
            err.contains("does not advertise remote action"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_advertised_remote_capabilities_lists_all_handlers() {
        assert_eq!(
            advertised_remote_capabilities(),
            vec![
                "launch",
                "kill",
                "resume",
                "config_get",
                "config_set",
                "relay_off",
                "term_screen",
                "term_inject",
                "transcript",
                "events",
                "sub_create",
                "sub_list",
                "sub_unsub",
            ]
        );
    }

    #[test]
    fn test_handle_remote_config_get_blocks_relay_psk() {
        let db = test_db();
        let err = handle_remote_config_get(
            &db,
            &json!({"fields": ["relay_psk"]}),
            "initiator",
            &HcomConfig::default(),
        )
        .unwrap_err();
        assert_eq!(err, "relay_psk is not remotely queryable");
    }

    #[test]
    fn test_handle_remote_config_get_blocks_relay_psk_for_instance_mode() {
        let db = test_db();
        let err = handle_remote_config_get(
            &db,
            &json!({"instance": "luna", "field": "relay_psk"}),
            "initiator",
            &HcomConfig::default(),
        )
        .unwrap_err();
        assert_eq!(err, "relay_psk is not remotely queryable");
    }

    #[test]
    fn test_handle_remote_config_set_blocks_relay_psk() {
        let db = test_db();
        let err = handle_remote_config_set(
            &db,
            &json!({"field": "relay_psk", "value": "secret"}),
            "initiator",
            &HcomConfig::default(),
        )
        .unwrap_err();
        assert_eq!(err, "relay_psk is not remotely queryable");
    }

    #[test]
    fn test_handle_remote_config_set_blocks_relay_psk_for_instance_mode() {
        let db = test_db();
        let err = handle_remote_config_set(
            &db,
            &json!({"instance": "luna", "field": "relay_psk", "value": "secret"}),
            "initiator",
            &HcomConfig::default(),
        )
        .unwrap_err();
        assert_eq!(err, "relay_psk is not remotely queryable");
    }

    fn seed_events(db: &HcomDb, count: usize) {
        for i in 0..count {
            let etype = if i % 2 == 0 { "message" } else { "status" };
            db.log_event(etype, "luna", &json!({"i": i})).unwrap();
        }
    }

    #[test]
    fn test_handle_remote_events_empty_filters_returns_all() {
        let db = test_db();
        seed_events(&db, 5);
        let out =
            handle_remote_events(&db, &json!({}), "initiator", &HcomConfig::default()).unwrap();
        assert_eq!(out["count"].as_u64().unwrap(), 5);
        let events = out["events"].as_array().unwrap();
        assert_eq!(events.len(), 5);
    }

    #[test]
    fn test_handle_remote_events_type_message_filter() {
        let db = test_db();
        seed_events(&db, 6);
        let out = handle_remote_events(
            &db,
            &json!({"filters": {"type": ["message"]}, "last": 50}),
            "initiator",
            &HcomConfig::default(),
        )
        .unwrap();
        let events = out["events"].as_array().unwrap();
        assert_eq!(events.len(), 3);
        for e in events {
            assert_eq!(e["type"].as_str().unwrap(), "message");
        }
    }

    #[test]
    fn test_handle_remote_events_hard_cap() {
        let db = test_db();
        seed_events(&db, 10);
        let out = handle_remote_events(
            &db,
            &json!({"last": 9999}),
            "initiator",
            &HcomConfig::default(),
        )
        .unwrap();
        assert_eq!(out["count"].as_u64().unwrap(), 10);
        let out = handle_remote_events(
            &db,
            &json!({"last": 3}),
            "initiator",
            &HcomConfig::default(),
        )
        .unwrap();
        assert_eq!(out["count"].as_u64().unwrap(), 3);
    }

    #[test]
    fn test_handle_remote_events_missing_filters_ok() {
        let db = test_db();
        seed_events(&db, 2);
        let out = handle_remote_events(
            &db,
            &json!({"last": 10}),
            "initiator",
            &HcomConfig::default(),
        )
        .unwrap();
        assert_eq!(out["count"].as_u64().unwrap(), 2);
    }

    #[test]
    fn test_handle_remote_events_truncates_when_envelope_exceeds_cap() {
        let db = test_db();
        // Big payload per event so a few rows blow past the 96 KiB cap.
        let big = "x".repeat(8_000);
        for _ in 0..20 {
            db.log_event("message", "luna", &json!({"blob": big}))
                .unwrap();
        }
        let input_count = 20usize;
        let out = handle_remote_events(
            &db,
            &json!({"last": input_count}),
            "initiator",
            &HcomConfig::default(),
        )
        .unwrap();
        assert_eq!(out["truncated"].as_bool(), Some(true));
        let returned = out["events"].as_array().unwrap().len();
        assert!(
            returned < input_count,
            "expected truncation, got {returned}"
        );
        let envelope_len = serde_json::to_string(&out).unwrap().len();
        assert!(
            envelope_len <= REMOTE_EVENTS_BYTE_CAP,
            "envelope {envelope_len} bytes exceeds cap"
        );
    }

    #[test]
    fn test_handle_remote_events_invalid_sql_returns_err() {
        let db = test_db();
        seed_events(&db, 2);
        let err = handle_remote_events(
            &db,
            &json!({"sql": "not a real column = 1"}),
            "initiator",
            &HcomConfig::default(),
        )
        .unwrap_err();
        assert!(err.contains("sql error"), "unexpected err: {err}");
    }
}
