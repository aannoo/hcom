use std::time::Instant;

use serde_json::Value;

use crate::bootstrap;
use crate::db::HcomDb;
use crate::instance_binding;
use crate::instance_lifecycle as lifecycle;
use crate::instances;
use crate::log::{log_error, log_info};
use crate::shared::ST_LISTENING;
use crate::shared::context::HcomContext;

use super::common;
use super::common::finalize_session;

fn parse_flag(argv: &[String], flag: &str) -> Option<String> {
    argv.iter()
        .position(|a| a == flag)
        .and_then(|i| argv.get(i + 1))
        .cloned()
}

fn has_flag(argv: &[String], flag: &str) -> bool {
    argv.iter().any(|a| a == flag)
}

fn parse_value_arg(argv: &[String], flags: &[&str]) -> Option<String> {
    for (idx, token) in argv.iter().enumerate() {
        for flag in flags {
            if token == flag {
                return argv.get(idx + 1).cloned();
            }
            let prefix = format!("{flag}=");
            if let Some(value) = token.strip_prefix(&prefix) {
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

fn parse_launch_model(raw: &str) -> Option<Value> {
    let (provider_id, model_id) = raw.split_once('/')?;
    if provider_id.is_empty() || model_id.is_empty() {
        return None;
    }
    Some(serde_json::json!({
        "providerID": provider_id,
        "modelID": model_id,
    }))
}

fn launch_agent_and_model_from_args(launch_args: Option<&str>) -> (Option<String>, Option<Value>) {
    let Some(raw_args) = launch_args.filter(|value| !value.is_empty()) else {
        return (None, None);
    };
    let argv: Vec<String> = match serde_json::from_str(raw_args) {
        Ok(args) => args,
        Err(_) => return (None, None),
    };
    let agent = parse_value_arg(&argv, &["--agent"]);
    let model =
        parse_value_arg(&argv, &["--model", "-m"]).and_then(|value| parse_launch_model(&value));
    (agent, model)
}

fn launch_agent_and_model(db: &HcomDb, instance_name: &str) -> (Option<String>, Option<Value>) {
    db.get_instance_full(instance_name)
        .ok()
        .flatten()
        .map(|instance| launch_agent_and_model_from_args(instance.launch_args.as_deref()))
        .unwrap_or((None, None))
}

fn upsert_plugin_notify_endpoint(db: &HcomDb, instance_name: &str, port: u16) {
    if let Err(e) = db.upsert_notify_endpoint(instance_name, "plugin", port) {
        log_error(
            "cline",
            "notify.upsert.failed",
            &format!("instance={instance_name} port={port} error={e}"),
        );
    }
}

fn notify_all_endpoints(db: &HcomDb, instance_name: &str) {
    lifecycle::notify_instance_endpoints(db, instance_name, &[]);
}

fn get_cline_db_path() -> Option<String> {
    let xdg_data = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{}/.local/share", home)
    });
    let db_path = std::path::PathBuf::from(&xdg_data)
        .join("cline")
        .join("cline.db");
    if db_path.exists() {
        Some(db_path.to_string_lossy().to_string())
    } else {
        None
    }
}

pub const TASK_START_SOURCE: &str = include_str!("../opencode_plugin/TaskStart");
pub const TASK_RESUME_SOURCE: &str = include_str!("../opencode_plugin/TaskResume");
pub const TASK_COMPLETE_SOURCE: &str = include_str!("../opencode_plugin/TaskComplete");
pub const TASK_CANCEL_SOURCE: &str = include_str!("../opencode_plugin/TaskCancel");
pub const USER_PROMPT_SUBMIT_SOURCE: &str = include_str!("../opencode_plugin/UserPromptSubmit");

const PLUGIN_FILES: &[(&str, &str)] = &[
    ("TaskStart", TASK_START_SOURCE),
    ("TaskResume", TASK_RESUME_SOURCE),
    ("TaskComplete", TASK_COMPLETE_SOURCE),
    ("TaskCancel", TASK_CANCEL_SOURCE),
    ("UserPromptSubmit", USER_PROMPT_SUBMIT_SOURCE),
];

fn current_home_dir() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default())
}

fn xdg_config_home() -> String {
    std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{}/.config", home)
    })
}

pub fn get_cline_plugin_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(xdg_config_home())
        .join("cline")
        .join("hcom")
}

fn scan_plugin_dirs() -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();
    let xdg_base = std::path::PathBuf::from(xdg_config_home()).join("cline");
    candidates.push(xdg_base.join("hcom"));
    candidates.push(xdg_base.join("plugins"));
    candidates.push(xdg_base.join("hooks"));

    let tool_root = crate::runtime_env::tool_config_root();
    let home = current_home_dir();
    if tool_root != home {
        let tool_base = tool_root.join(".cline");
        candidates.push(tool_base.join("hcom"));
        candidates.push(tool_base.join("plugins"));
        candidates.push(tool_base.join("hooks"));
    }

    let mut deduped = Vec::new();
    for dir in candidates.into_iter().filter(|d| d.exists()) {
        if !deduped.contains(&dir) {
            deduped.push(dir);
        }
    }
    deduped
}

fn plugin_files_match(dir: &std::path::Path) -> bool {
    for (name, source) in PLUGIN_FILES {
        let path = dir.join(name);
        match std::fs::read_to_string(&path) {
            Ok(content) if content == *source => {}
            _ => return false,
        }
    }
    true
}

pub fn verify_cline_plugin_installed() -> bool {
    let primary = get_cline_plugin_dir();
    if plugin_files_match(&primary) {
        return true;
    }
    scan_plugin_dirs()
        .iter()
        .any(|dir| plugin_files_match(dir))
}

pub fn install_cline_plugin() -> std::io::Result<bool> {
    let target_dir = get_cline_plugin_dir();
    std::fs::create_dir_all(&target_dir)?;

    for (name, source) in PLUGIN_FILES {
        let path = target_dir.join(name);
        std::fs::write(&path, source)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(true)
}

pub fn remove_cline_plugin() -> std::io::Result<()> {
    let mut dirs = vec![get_cline_plugin_dir()];
    let xdg_base = std::path::PathBuf::from(xdg_config_home()).join("cline");
    for sub in &["hcom", "plugins", "hooks"] {
        let p = xdg_base.join(sub);
        if !dirs.contains(&p) {
            dirs.push(p);
        }
    }
    let tool_root = crate::runtime_env::tool_config_root();
    let home = current_home_dir();
    if tool_root != home {
        let tool_base = tool_root.join(".cline");
        for sub in &["hcom", "plugins", "hooks"] {
            let p = tool_base.join(sub);
            if !dirs.contains(&p) {
                dirs.push(p);
            }
        }
    }

    for dir in dirs {
        if dir.exists() {
            for (name, _) in PLUGIN_FILES {
                let path = dir.join(name);
                if path.exists() {
                    std::fs::remove_file(&path)?;
                }
            }
            std::fs::remove_dir(&dir).ok();
        }
    }
    Ok(())
}

pub fn ensure_plugin_installed() -> bool {
    if verify_cline_plugin_installed() {
        return true;
    }
    install_cline_plugin().unwrap_or(false)
}



fn handle_start(ctx: &HcomContext, db: &HcomDb, argv: &[String]) -> (i32, String) {
    let session_id = match parse_flag(argv, "--session-id") {
        Some(sid) => sid,
        None => return (0, r#"{"error":"Missing --session-id"}"#.to_string()),
    };

    let notify_port: Option<u16> = parse_flag(argv, "--notify-port").and_then(|s| s.parse().ok());

    let process_id = match &ctx.process_id {
        Some(pid) => pid.clone(),
        None => return (0, r#"{"error":"HCOM_PROCESS_ID not set"}"#.to_string()),
    };

    if let Ok(Some(existing_name)) = db.get_session_binding(&session_id) {
        let mut rebind_updates = serde_json::Map::new();
        rebind_updates.insert("name_announced".into(), serde_json::json!(false));
        rebind_updates.insert("session_id".into(), serde_json::json!(&session_id));

        if let Some(db_path) = get_cline_db_path() {
            rebind_updates.insert("transcript_path".into(), serde_json::json!(db_path));
        }

        instances::update_instance_position(db, &existing_name, &rebind_updates);
        lifecycle::set_status(
            db,
            &existing_name,
            ST_LISTENING,
            "start",
            Default::default(),
        );

        let hcom_config = crate::config::HcomConfig::load(None).unwrap_or_default();
        let bootstrap_text = bootstrap::get_bootstrap(
            db,
            &ctx.hcom_dir,
            &existing_name,
            "cline",
            ctx.is_background,
            ctx.is_launched,
            &ctx.notes,
            &hcom_config.tag,
            crate::relay::is_relay_enabled(&hcom_config),
            ctx.background_name.as_deref(),
        );

        if let Some(port) = notify_port {
            upsert_plugin_notify_endpoint(db, &existing_name, port);
        }

        log_info(
            "hooks",
            "cline-start.rebind",
            &format!("instance={} session_id={}", existing_name, session_id),
        );

        let (launch_agent, launch_model) = launch_agent_and_model(db, &existing_name);
        let mut result = serde_json::json!({
            "name": existing_name,
            "session_id": session_id,
        });
        result["bootstrap"] = Value::String(bootstrap_text);
        if let Some(agent) = launch_agent {
            result["agent"] = Value::String(agent);
        }
        if let Some(model) = launch_model {
            result["model"] = model;
        }
        return (0, serde_json::to_string(&result).unwrap_or_default());
    }

    let instance_name =
        match instance_binding::bind_session_to_process(db, &session_id, Some(&process_id)) {
            Some(name) => name,
            None => {
                return (
                    0,
                    r#"{"error":"No instance bound to this process"}"#.to_string(),
                );
            }
        };

    if let Err(e) = db.rebind_instance_session(&instance_name, &session_id) {
        log_error(
            "hooks",
            "hook.error",
            &format!("hook=cline-start op=rebind_session err={}", e),
        );
    }

    if let Ok(Some(existing)) = db.get_instance_full(&instance_name) {
        if existing.last_event_id == 0 {
            let launch_event_id: Option<i64> = std::env::var("HCOM_LAUNCH_EVENT_ID")
                .ok()
                .and_then(|s| s.parse().ok());
            let current_max = db.get_last_event_id();
            let new_id = match launch_event_id {
                Some(lei) if lei <= current_max => lei,
                _ => current_max,
            };
            let mut id_updates = serde_json::Map::new();
            id_updates.insert("last_event_id".into(), serde_json::json!(new_id));
            instances::update_instance_position(db, &instance_name, &id_updates);
        }
    }

    lifecycle::set_status(
        db,
        &instance_name,
        ST_LISTENING,
        "start",
        Default::default(),
    );

    instance_binding::capture_and_store_launch_context(db, &instance_name);

    let mut updates = serde_json::Map::new();
    updates.insert("session_id".into(), serde_json::json!(&session_id));
    if let Some(db_path) = get_cline_db_path() {
        updates.insert("transcript_path".into(), serde_json::json!(db_path));
    }
    if !ctx.cwd.as_os_str().is_empty() {
        updates.insert(
            "directory".into(),
            serde_json::json!(ctx.cwd.to_string_lossy()),
        );
    }
    instances::update_instance_position(db, &instance_name, &updates);

    if let Some(port) = notify_port {
        upsert_plugin_notify_endpoint(db, &instance_name, port);
    }

    let tag = db
        .get_instance_full(&instance_name)
        .ok()
        .flatten()
        .and_then(|d| d.tag.clone())
        .unwrap_or_default();

    let hcom_config = crate::config::HcomConfig::load(None).unwrap_or_default();
    let relay_enabled = crate::relay::is_relay_enabled(&hcom_config);
    let effective_tag = if tag.is_empty() {
        &hcom_config.tag
    } else {
        &tag
    };
    let bootstrap_text = bootstrap::get_bootstrap(
        db,
        &ctx.hcom_dir,
        &instance_name,
        "cline",
        ctx.is_background,
        ctx.is_launched,
        &ctx.notes,
        effective_tag,
        relay_enabled,
        ctx.background_name.as_deref(),
    );

    crate::relay::worker::ensure_worker(true);

    let (launch_agent, launch_model) = launch_agent_and_model(db, &instance_name);
    let mut response = serde_json::json!({
        "name": instance_name,
        "session_id": session_id,
    });
    response["bootstrap"] = Value::String(bootstrap_text);
    if let Some(agent) = launch_agent {
        response["agent"] = Value::String(agent);
    }
    if let Some(model) = launch_model {
        response["model"] = model;
    }
    (0, serde_json::to_string(&response).unwrap_or_default())
}

fn handle_status(db: &HcomDb, argv: &[String]) -> (i32, String) {
    let name = match parse_flag(argv, "--name") {
        Some(n) => n,
        None => return (0, r#"{"error":"Missing --name or --status"}"#.to_string()),
    };
    let status = match parse_flag(argv, "--status") {
        Some(s) => s,
        None => return (0, r#"{"error":"Missing --name or --status"}"#.to_string()),
    };
    let context = parse_flag(argv, "--context").unwrap_or_default();
    let detail = parse_flag(argv, "--detail").unwrap_or_default();
    lifecycle::set_status(
        db,
        &name,
        &status,
        &context,
        lifecycle::StatusUpdate {
            detail: &detail,
            ..Default::default()
        },
    );
    if status == ST_LISTENING {
        notify_all_endpoints(db, &name);
    }
    (0, r#"{"ok":true}"#.to_string())
}

fn handle_read(db: &HcomDb, argv: &[String]) -> (i32, String) {
    let name = match parse_flag(argv, "--name") {
        Some(n) => n,
        None => return (0, r#"{"error":"Missing --name"}"#.to_string()),
    };

    let format_mode = has_flag(argv, "--format");
    let check_mode = has_flag(argv, "--check");
    let ack_mode = has_flag(argv, "--ack");

    let raw_messages = db.get_unread_messages(&name);
    let messages: Vec<Value> = raw_messages.iter().map(common::message_to_value).collect();

    if format_mode {
        if messages.is_empty() {
            return (0, String::new());
        }
        let deliver = common::limit_delivery_messages(&messages);
        // Auto-ack: advance cursor so same messages aren't re-delivered
        let last_id = deliver
            .iter()
            .filter_map(|m| m.get("event_id").and_then(|v| v.as_i64()))
            .max()
            .unwrap_or(0);
        if last_id > 0 {
            let mut updates = serde_json::Map::new();
            updates.insert("last_event_id".into(), serde_json::json!(last_id));
            instances::update_instance_position(db, &name, &updates);
        }
        let formatted = common::format_messages_json_for_instance(db, &deliver, &name);
        return (0, formatted);
    }

    if ack_mode {
        let up_to = parse_flag(argv, "--up-to");
        if let Some(up_to_str) = up_to {
            let ack_id: i64 = match up_to_str.parse() {
                Ok(id) => id,
                Err(_) => {
                    return (
                        0,
                        serde_json::json!({"error": format!("Invalid --up-to: {}", up_to_str)})
                            .to_string(),
                    );
                }
            };
            let mut updates = serde_json::Map::new();
            updates.insert("last_event_id".into(), serde_json::json!(ack_id));
            instances::update_instance_position(db, &name, &updates);
            return (0, serde_json::json!({"acked_to": ack_id}).to_string());
        }
        if messages.is_empty() {
            return (0, r#"{"acked":0}"#.to_string());
        }
        let last_id = messages
            .iter()
            .filter_map(|m| m.get("event_id").and_then(|v| v.as_i64()))
            .max()
            .unwrap_or(0);
        let ack_id = if last_id > 0 {
            last_id
        } else {
            db.get_last_event_id()
        };
        if ack_id > 0 {
            let mut updates = serde_json::Map::new();
            updates.insert("last_event_id".into(), serde_json::json!(ack_id));
            instances::update_instance_position(db, &name, &updates);
        }
        return (0, serde_json::json!({"acked": messages.len()}).to_string());
    }

    if check_mode {
        return (
            0,
            if messages.is_empty() { "false" } else { "true" }.to_string(),
        );
    }

    (
        0,
        serde_json::to_string(&messages).unwrap_or_else(|_| "[]".to_string()),
    )
}

fn handle_stop(db: &HcomDb, argv: &[String]) -> (i32, String) {
    let name = match parse_flag(argv, "--name") {
        Some(n) => n,
        None => return (0, r#"{"error":"Missing --name"}"#.to_string()),
    };
    let reason = parse_flag(argv, "--reason").unwrap_or_else(|| "unknown".to_string());
    finalize_session(db, &name, &reason, None);
    (0, r#"{"ok":true}"#.to_string())
}

pub fn dispatch_cline_hook(hook_name: &str, argv: &[String]) -> (i32, String) {
    let start = Instant::now();
    let ctx = HcomContext::from_os();
    crate::paths::ensure_hcom_directories_at(&ctx.hcom_dir);
    let db = match HcomDb::open() {
        Ok(db) => db,
        Err(e) => {
            log_error("hooks", "hook.error",
                &format!("hook={} op=db_open err={}", hook_name, e));
            return (0, serde_json::json!({"error": format!("DB open failed: {}", e)}).to_string());
        }
    };
    if !common::hook_gate_check(&ctx, &db) {
        return (0, String::new());
    }
    let handler_argv: Vec<String> = if !argv.is_empty() && argv[0] == hook_name {
        argv[1..].to_vec()
    } else {
        argv.to_vec()
    };
    let handler_start = Instant::now();
    let hook_name_owned = hook_name.to_string();
    let (exit_code, output) = common::dispatch_with_panic_guard(
        "cline",
        &hook_name_owned,
        (0, serde_json::json!({"error": "internal panic"}).to_string()),
        || match hook_name_owned.as_str() {
            "cline-start" => handle_start(&ctx, &db, &handler_argv),
            "cline-status" => handle_status(&db, &handler_argv),
            "cline-read" => handle_read(&db, &handler_argv),
            "cline-stop" => handle_stop(&db, &handler_argv),
            _ => (0, serde_json::json!({"error": format!("Unknown Cline hook: {}", hook_name_owned)}).to_string()),
        },
    );
    let handler_ms = handler_start.elapsed().as_secs_f64() * 1000.0;
    let total_ms = start.elapsed().as_secs_f64() * 1000.0;
    log_info("hooks", "cline.dispatch.timing",
        &format!("hook={} handler_ms={:.2} total_ms={:.2} exit_code={}",
            hook_name, handler_ms, total_ms, exit_code));
    (exit_code, output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_flag() {
        let argv = vec!["--name".to_string(), "luna".to_string()];
        assert_eq!(parse_flag(&argv, "--name"), Some("luna".to_string()));
        assert_eq!(parse_flag(&argv, "--status"), None);
    }

    #[test]
    fn test_has_flag() {
        let argv = vec!["--format".to_string()];
        assert!(has_flag(&argv, "--format"));
        assert!(!has_flag(&argv, "--check"));
    }

    #[test]
    fn test_parse_value_arg() {
        let argv = vec!["--model=sonnet".to_string()];
        assert_eq!(parse_value_arg(&argv, &["--model"]), Some("sonnet".to_string()));
    }

    #[test]
    fn test_parse_launch_model() {
        assert_eq!(
            parse_launch_model("anthropic/claude-sonnet-4-6"),
            Some(serde_json::json!({"providerID": "anthropic", "modelID": "claude-sonnet-4-6"}))
        );
        assert_eq!(parse_launch_model("invalid"), None);
    }
}
