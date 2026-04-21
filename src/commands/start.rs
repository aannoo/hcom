//! Start command: `hcom start [--as <name>] [--orphan <name|pid>]`
//!
//! Runs inside an already-running tool session rather than launching a new one.
//! Used for adhoc/manual setup, identity rebinding, and orphan recovery:
//! - Bare start: detect vanilla tool or create adhoc instance
//! - `--orphan`: recover orphaned PTY process
//! - `--as`: rebind session identity

use anyhow::{Result, bail};
use serde_json::json;
use std::collections::HashSet;
use std::path::PathBuf;

use crate::bootstrap;
use crate::config::HcomConfig;
use crate::db::HcomDb;
use crate::identity;
use crate::instance_binding;
use crate::instance_lifecycle as lifecycle;
use crate::instance_names;
use crate::instances;
use crate::log::log_info;
use crate::paths;
use crate::pidtrack;
use crate::relay;
use crate::router::GlobalFlags;
use crate::shared::constants::ST_ACTIVE;
use crate::shared::context::HcomContext;

/// Parsed arguments for `hcom start`.
#[derive(clap::Parser, Debug)]
#[command(name = "start", about = "Start hcom participation")]
pub struct StartArgs {
    /// Rebind to a different instance name
    #[arg(long = "as")]
    pub as_name: Option<String>,
    /// Recover orphaned PTY process by name or PID
    #[arg(long)]
    pub orphan: Option<String>,
}

/// Run the start command.
pub fn run(argv: &[String], flags: &GlobalFlags) -> Result<i32> {
    // Filter out global flags already consumed by the router (start, --name X, --go)
    let mut filtered = vec!["start".to_string()];
    let mut skip_next = false;
    for arg in argv {
        if skip_next {
            skip_next = false;
            continue;
        }
        match arg.as_str() {
            "start" | "--go" => continue,
            "--name" => {
                skip_next = true;
                continue;
            }
            _ => filtered.push(arg.clone()),
        }
    }

    use clap::Parser;
    let start_args = match StartArgs::try_parse_from(&filtered) {
        Ok(a) => a,
        Err(e) => {
            e.print().ok();
            return Ok(if e.use_stderr() { 1 } else { 0 });
        }
    };

    let orphan_target = start_args.orphan;
    let rebind_target = start_args.as_name;

    let db = HcomDb::open()?;
    let hcom_dir = paths::hcom_dir();

    let ctx = HcomContext::from_os();
    let instance_name = flags
        .name
        .as_deref()
        .map(|name| instances::resolve_display_name(&db, name).unwrap_or_else(|| name.to_string()));

    // BLOCK DURING ACTIVE TASKS: prevents subagents from corrupting parent/sibling instances.
    // When a subagent runs --as or bare start, process_id resolves to the parent which has
    // running_tasks.active=True. Only --name <agent_id> (explicit initiator) bypasses this gate.
    if rebind_target.is_some() || orphan_target.is_some() || instance_name.is_none() {
        if let Ok(ident) =
            identity::resolve_identity(&db, None, None, None, ctx.process_id.as_deref(), None, None)
        {
            if let Some(inst_data) = &ident.instance_data {
                let rt_str = inst_data
                    .get("running_tasks")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let rt = instances::parse_running_tasks(Some(rt_str));
                if rt.active {
                    if rebind_target.is_some() {
                        println!("[HCOM] Cannot use --as while Tasks are running.");
                    } else if orphan_target.is_some() {
                        println!("[HCOM] Cannot use --orphan while Tasks are running.");
                    } else {
                        println!(
                            "[HCOM] Cannot run 'hcom start' from within a Task subagent.\n\
                             Subagents must use: hcom start --name <your-agent-id>"
                        );
                    }
                    return Ok(1);
                }
            }
        }
    }

    // SUBAGENT DETECTION: check BOTH --name and --as for agent_id matches in running_tasks.
    // Must happen BEFORE --as handling to block subagents from picking new identities.
    // Check both independently: --as matching a subagent agent_id must be blocked,
    // --name matching triggers subagent registration.
    let subagent_via_name = instance_name
        .as_deref()
        .and_then(|id| detect_subagent(&db, id));
    let subagent_via_as = rebind_target
        .as_deref()
        .and_then(|id| detect_subagent(&db, id));

    if subagent_via_as.is_some() || (subagent_via_name.is_some() && rebind_target.is_some()) {
        println!("[HCOM] Subagents cannot change identity. End your turn.");
        return Ok(1);
    }
    let subagent_info = subagent_via_name;

    if let Some(orphan) = orphan_target {
        return start_from_orphan(&db, &hcom_dir, &orphan, &ctx);
    }

    if let Some(rebind) = rebind_target {
        return start_rebind(&db, &rebind, &ctx, instance_name.as_deref());
    }

    // Subagent registration path (--name <agent_id> that matched a parent's running_tasks)
    if let Some(info) = subagent_info {
        return start_subagent(&db, &info);
    }

    // Bare start: auto-detect tool or create adhoc instance
    start_bare(&db, &hcom_dir, &ctx, instance_name.as_deref())
}

/// Info about a detected subagent from a parent's running_tasks.
struct SubagentInfo {
    agent_id: String,
    agent_type: String,
    parent_name: String,
    parent_session_id: Option<String>,
    parent_tag: Option<String>,
}

/// Check if `check_id` matches an agent_id in any parent's running_tasks.subagents.
fn detect_subagent(db: &HcomDb, check_id: &str) -> Option<SubagentInfo> {
    // Query instances that have subagents tracked
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT name, session_id, tag, running_tasks FROM instances \
             WHERE running_tasks LIKE '%subagents%'",
        )
        .ok()?;

    let rows: Vec<(String, Option<String>, Option<String>, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .ok()?
        .filter_map(|r| r.ok())
        .collect();

    for (name, session_id, tag, rt_json) in &rows {
        let rt = instances::parse_running_tasks(Some(rt_json));
        for task in &rt.subagents {
            if task.get("agent_id").and_then(|v| v.as_str()) == Some(check_id) {
                let agent_type = task
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("task")
                    .to_string();
                return Some(SubagentInfo {
                    agent_id: check_id.to_string(),
                    agent_type,
                    parent_name: name.clone(),
                    parent_session_id: session_id.clone(),
                    parent_tag: tag.clone(),
                });
            }
        }
    }
    None
}

/// Path S: Subagent registration — create structured parent_type_N name.
fn start_subagent(db: &HcomDb, info: &SubagentInfo) -> Result<i32> {
    // Gate: subagents get ONE start. Any stop = permanently dead.
    let stopped_by: Option<String> = db
        .conn()
        .prepare(
            "SELECT json_extract(data, '$.by') FROM events \
             WHERE type = 'life' \
             AND json_extract(data, '$.action') = 'stopped' \
             AND json_extract(data, '$.snapshot.agent_id') = ? \
             ORDER BY timestamp DESC LIMIT 1",
        )?
        .query_row(rusqlite::params![info.agent_id], |row| row.get(0))
        .ok();

    if let Some(by) = stopped_by {
        let by = if by.is_empty() {
            "system".to_string()
        } else {
            by
        };
        println!(
            "[HCOM] Your session was stopped by {by}. Do not continue working. End your turn immediately."
        );
        return Ok(1);
    }

    // Check if instance already exists by agent_id (reuse name)
    let existing_name: Option<String> = db
        .conn()
        .query_row(
            "SELECT name FROM instances WHERE agent_id = ?",
            rusqlite::params![info.agent_id],
            |row| row.get(0),
        )
        .ok();

    if let Some(name) = existing_name {
        lifecycle::set_status(db, &name, ST_ACTIVE, "start", Default::default());
        println!("hcom already started for {name}");
        return Ok(0);
    }

    // Sanitize agent_type: keep only [a-z0-9_]
    let sanitized_type: String = info
        .agent_type
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let sanitized_type = sanitized_type.trim_matches('_');
    let sanitized_type = if sanitized_type.is_empty() {
        "task"
    } else {
        sanitized_type
    };

    // Compute next suffix: query max(n) for parent_type_% pattern
    let pattern = format!("{}_{}_", info.parent_name, sanitized_type);
    let like_pattern = format!("{}%", pattern);
    let mut stmt = db
        .conn()
        .prepare("SELECT name FROM instances WHERE name LIKE ?")?;
    let names: Vec<String> = stmt
        .query_map(rusqlite::params![like_pattern], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    let mut max_n: u32 = 0;
    for name in &names {
        if let Some(suffix) = name.strip_prefix(&pattern) {
            if let Ok(n) = suffix.parse::<u32>() {
                max_n = max_n.max(n);
            }
        }
    }

    // pattern = "parent_type_", so subagent_name = "parent_type_N"
    let subagent_name = format!("{}{}", pattern, max_n + 1);

    let initial_event_id = db.get_last_event_id();
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let now = crate::shared::time::now_epoch_f64();

    // Direct DB insert with agent_id and parent fields
    let insert_result = db.conn().execute(
        "INSERT INTO instances \
         (name, session_id, parent_session_id, parent_name, tag, agent_id, \
          created_at, last_event_id, directory, last_stop, status) \
         VALUES (?, NULL, ?, ?, ?, ?, ?, ?, ?, 0, 'active')",
        rusqlite::params![
            subagent_name,
            info.parent_session_id,
            info.parent_name,
            info.parent_tag,
            info.agent_id,
            now,
            initial_event_id,
            cwd,
        ],
    );

    // On name collision (constraint violation), retry once with next suffix.
    // Other DB errors propagate immediately.
    let subagent_name = match insert_result {
        Ok(_) => subagent_name,
        Err(rusqlite::Error::SqliteFailure(err, _))
            if err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            let retry_name = format!("{}{}", pattern, max_n + 2);
            db.conn()
                .execute(
                    "INSERT INTO instances \
                 (name, session_id, parent_session_id, parent_name, tag, agent_id, \
                  created_at, last_event_id, directory, last_stop, status) \
                 VALUES (?, NULL, ?, ?, ?, ?, ?, ?, ?, 0, 'active')",
                    rusqlite::params![
                        retry_name,
                        info.parent_session_id,
                        info.parent_name,
                        info.parent_tag,
                        info.agent_id,
                        now,
                        initial_event_id,
                        cwd,
                    ],
                )
                .map_err(|e| {
                    anyhow::anyhow!("Failed to create unique subagent name after retry: {e}")
                })?;
            retry_name
        }
        Err(e) => return Err(anyhow::anyhow!("Failed to insert subagent instance: {e}")),
    };

    // Capture launch context
    instance_binding::capture_and_store_launch_context(db, &subagent_name);

    // Set active status (logs life event)
    lifecycle::set_status(
        db,
        &subagent_name,
        ST_ACTIVE,
        "tool:start",
        Default::default(),
    );

    log_info(
        "lifecycle",
        "start.subagent",
        &format!(
            "name={} parent={} agent_id={} agent_type={}",
            subagent_name, info.parent_name, info.agent_id, info.agent_type
        ),
    );

    // Print subagent bootstrap
    let bootstrap = bootstrap::get_subagent_bootstrap(&subagent_name, &info.parent_name);
    if !bootstrap.is_empty() {
        println!("{bootstrap}");
    }

    Ok(0)
}

/// Recover orphaned PTY process by PID or name.
fn start_from_orphan(
    db: &HcomDb,
    hcom_dir: &std::path::Path,
    target: &str,
    _ctx: &HcomContext,
) -> Result<i32> {
    let active_pids: HashSet<u32> = db
        .iter_instances_full()?
        .iter()
        .filter_map(|inst| inst.pid.map(|p| p as u32))
        .collect();
    let orphans = pidtrack::get_orphan_processes(hcom_dir, Some(&active_pids));

    if orphans.is_empty() {
        bail!("No orphan processes found.");
    }

    // Match by PID or name
    let orphan = if let Ok(pid) = target.parse::<u32>() {
        match orphans.iter().find(|o| o.pid == pid) {
            Some(o) => o,
            None => bail!("Orphan PID {} not found.", pid),
        }
    } else {
        let matches: Vec<_> = orphans
            .iter()
            .filter(|o| o.names.contains(&target.to_string()))
            .collect();
        match matches.len() {
            0 => bail!("Orphan '{}' not found.", target),
            1 => matches[0],
            _ => {
                let pids: Vec<String> = matches.iter().map(|m| m.pid.to_string()).collect();
                bail!(
                    "Multiple orphans match '{}' (PIDs: {}). Use --orphan <pid>.",
                    target,
                    pids.join(", ")
                );
            }
        }
    };

    let pid = orphan.pid;

    if orphan.process_id.is_empty() {
        bail!(
            "Orphan PID {} has no process_id and cannot be recovered.",
            pid
        );
    }

    let preferred_name = orphan.names.last().cloned().unwrap_or_default();
    let can_reuse = !preferred_name.is_empty()
        && identity::is_valid_base_name(&preferred_name)
        && db.get_instance_full(&preferred_name)?.is_none();
    let name = if can_reuse {
        preferred_name
    } else {
        instance_names::generate_unique_name(db)?
    };

    // Core DB registration
    let _ = pidtrack::recover_single_orphan_to_db(db, orphan, &name);

    db.log_event(
        "life",
        &name,
        &json!({
            "action": "started",
            "by": "cli",
            "reason": "orphan_recover",
            "orphan_pid": pid,
        }),
    )
    .ok();

    pidtrack::remove_pid(hcom_dir, pid);

    println!("[hcom:{}]", name);
    if can_reuse {
        println!("Recovered orphan PID {} as '{}'.", pid, name);
    } else {
        println!(
            "Recovered orphan PID {} as new identity '{}' (name conflict/unavailable).",
            pid, name
        );
    }

    log_info(
        "start",
        "orphan.recovered",
        &format!("name={} pid={} tool={}", name, pid, orphan.tool),
    );

    Ok(0)
}

/// Rebind session identity (`--as <name>`), preserving last_event_id.
fn start_rebind(
    db: &HcomDb,
    rebind_target: &str,
    ctx: &HcomContext,
    explicit_name: Option<&str>,
) -> Result<i32> {
    let hcom_dir = paths::hcom_dir();

    // Resolve the target name
    let target_name = instances::resolve_display_name_or_stopped(db, rebind_target)
        .unwrap_or_else(|| rebind_target.to_string());

    let current_name = explicit_name.unwrap_or("");

    // Resolve session_id from process binding or existing instance
    let mut session_id: Option<String> = None;
    if let Some(ref process_id) = ctx.process_id {
        if let Ok(Some((sid, _))) = db.get_process_binding_full(process_id) {
            session_id = sid.filter(|s| !s.is_empty());
        }
    }
    if session_id.is_none() && !current_name.is_empty() {
        if let Ok(Some(current_data)) = db.get_instance_full(current_name) {
            session_id = current_data.session_id.filter(|s| !s.is_empty());
        }
    }

    let target_meta = load_rebind_target_metadata(db, &target_name).ok();
    if let Some(ref meta) = target_meta {
        ensure_rebind_compatible(&target_name, meta, ctx)?;
    }

    // Preserve last_event_id from target (cursor preservation)
    let mut last_event_id = target_meta.as_ref().map(|m| m.last_event_id);
    let target_data = db.get_instance_full(&target_name)?;

    // Final fallback: use current max to avoid re-delivering old messages
    if last_event_id.is_none() {
        last_event_id = Some(db.get_last_event_id());
    }

    // Skip delete for remote instances (origin_device_id)
    if let Some(ref td) = target_data {
        if td.origin_device_id.is_none() || td.origin_device_id.as_deref() == Some("") {
            if let Err(e) = db.delete_instance(&target_name) {
                eprintln!("[hcom] warn: delete_instance failed for {target_name}: {e}");
            }
        }
    }

    // Clean up target's bindings
    if let Err(e) = db.delete_process_bindings_for_instance(&target_name) {
        eprintln!("[hcom] warn: delete_process_bindings failed for {target_name}: {e}");
    }
    if let Err(e) = db.delete_session_bindings_for_instance(&target_name) {
        eprintln!("[hcom] warn: delete_session_bindings failed for {target_name}: {e}");
    }

    // Delete old identity if different from target
    if !current_name.is_empty() && current_name != target_name {
        if let Err(e) = db.delete_instance(current_name) {
            eprintln!("[hcom] warn: delete_instance failed for {current_name}: {e}");
        }
    }

    // Create fresh instance with the target name
    let tool = ctx.tool.as_str();
    let cwd_override = ctx.cwd.to_string_lossy().to_string();
    instance_binding::initialize_instance_in_position_file(
        db,
        &target_name,
        session_id.as_deref(),
        None, // parent_session_id
        None, // parent_name
        None, // agent_id
        None, // transcript_path
        Some(tool),
        false, // background
        None,  // tag
        None,  // wait_timeout
        None,  // subagent_timeout
        None,  // hints
        Some(&cwd_override),
    );

    // Restore cursor position + mark as announced
    {
        let mut updates = serde_json::Map::new();
        if let Some(eid) = last_event_id {
            updates.insert("last_event_id".into(), serde_json::json!(eid));
        }
        updates.insert("name_announced".into(), serde_json::json!(1));
        if let Err(e) = db.update_instance_fields(&target_name, &updates) {
            eprintln!("[hcom] warn: update_instance_fields failed for {target_name}: {e}");
        }
    }

    // Create bindings
    if let Some(ref sid) = session_id {
        if let Err(e) = db.set_session_binding(sid, &target_name) {
            eprintln!("[hcom] warn: set_session_binding failed for {target_name}: {e}");
        }
    }
    if let Some(ref process_id) = ctx.process_id {
        let sid = session_id.as_deref().unwrap_or("");
        if let Err(e) = db.set_process_binding(process_id, sid, &target_name) {
            eprintln!("[hcom] warn: set_process_binding failed for {target_name}: {e}");
        }

        // Migrate notify endpoints before notify so wake reaches correct port
        if !current_name.is_empty() && current_name != target_name {
            if let Err(e) = db.migrate_notify_endpoints(current_name, &target_name) {
                eprintln!("[hcom] warn: migrate_notify_endpoints failed: {e}");
            }
        }

        let _ = lifecycle::notify_instance_with_db(db, &target_name);
    }

    // Print bootstrap
    let hcom_config = HcomConfig::load(None).unwrap_or_else(|_| {
        let mut c = HcomConfig::default();
        c.normalize();
        c
    });

    let bootstrap_text = bootstrap::get_bootstrap(
        db,
        &hcom_dir,
        &target_name,
        tool,
        false,
        false,
        &ctx.notes,
        &hcom_config.tag,
        relay::is_relay_enabled(&hcom_config),
        None,
    );

    println!("[hcom:{}]", target_name);
    println!("{}", bootstrap_text);

    log_info(
        "start",
        "rebind.complete",
        &format!("from={} to={}", current_name, target_name),
    );

    Ok(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RebindTargetMetadata {
    tool: String,
    directory: String,
    last_event_id: i64,
}

fn ensure_rebind_compatible(
    target_name: &str,
    meta: &RebindTargetMetadata,
    ctx: &HcomContext,
) -> Result<()> {
    let current_tool = ctx.tool.as_str();
    if !meta.tool.is_empty() && meta.tool != current_tool {
        bail!(
            "Refusing to reclaim '{target_name}': latest identity used tool '{}' but current session is '{}'",
            meta.tool,
            current_tool
        );
    }

    let current_dir = ctx.cwd.to_string_lossy();
    if !meta.directory.is_empty() && !same_path(&meta.directory, &current_dir) {
        bail!(
            "Refusing to reclaim '{target_name}': latest identity used directory '{}' but current session is '{}'",
            meta.directory,
            current_dir
        );
    }

    Ok(())
}

fn same_path(left: &str, right: &str) -> bool {
    normalize_path_for_compare(left) == normalize_path_for_compare(right)
}

fn normalize_path_for_compare(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

/// Load rebind metadata from the live row first, then the latest stopped snapshot.
fn load_rebind_target_metadata(db: &HcomDb, name: &str) -> Result<RebindTargetMetadata> {
    if let Some(inst) = db.get_instance_full(name)? {
        return Ok(RebindTargetMetadata {
            tool: inst.tool,
            directory: inst.directory,
            last_event_id: inst.last_event_id,
        });
    }

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
                    return Ok(RebindTargetMetadata {
                        tool: snapshot
                            .get("tool")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        directory: snapshot
                            .get("directory")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        last_event_id: snapshot
                            .get("last_event_id")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0),
                    });
                }
            }
        }
    }

    bail!("No rebind metadata found for '{}'", name)
}

/// Path C: Bare start — detect tool or create adhoc instance.
fn start_bare(
    db: &HcomDb,
    hcom_dir: &std::path::Path,
    ctx: &HcomContext,
    explicit_name: Option<&str>,
) -> Result<i32> {
    let explicit_name = explicit_name
        .map(|name| instances::resolve_display_name(db, name).unwrap_or_else(|| name.to_string()));
    let explicit_name = explicit_name.as_deref();

    // Skip vanilla detection if --name is provided with an existing instance
    let has_valid_identity = explicit_name
        .and_then(|n| db.get_instance_full(n).ok().flatten())
        .is_some();

    // Vanilla tool detection: auto-install hooks for unmanaged AI tools
    if !has_valid_identity {
        if let Some(vanilla_tool) = ctx.detect_vanilla_tool() {
            // Auto-install hooks if missing
            let hooks_installed = match vanilla_tool {
                "claude" => crate::hooks::claude::verify_claude_hooks_installed(None, false),
                "gemini" => crate::hooks::gemini::verify_gemini_hooks_installed(false),
                "codex" => crate::hooks::codex::verify_codex_hooks_installed(false),
                _ => true,
            };
            if !hooks_installed {
                let tool_display = match vanilla_tool {
                    "claude" => "Claude Code",
                    "gemini" => "Gemini CLI",
                    "codex" => "Codex",
                    _ => vanilla_tool,
                };
                println!("Installing {} hooks...", vanilla_tool);
                let include_perms = crate::config::load_config_snapshot().core.auto_approve;
                let ok = match vanilla_tool {
                    "claude" => crate::hooks::claude::setup_claude_hooks(include_perms),
                    "gemini" => crate::hooks::gemini::setup_gemini_hooks(include_perms),
                    "codex" => crate::hooks::codex::setup_codex_hooks(include_perms),
                    _ => false,
                };
                if ok {
                    println!("\nRestart {tool_display} to enable automatic message delivery.");
                    println!("Then run: hcom start");
                } else {
                    eprintln!("Failed to install hooks. Run: hcom hooks add {vanilla_tool}");
                }
                return Ok(1);
            }

            // Gemini: ensure hooksConfig.enabled is set (self-heal for v0.26.0+)
            if vanilla_tool == "gemini" {
                let _ = crate::hooks::gemini::ensure_hooks_enabled();
            }
        }
    }

    let tool = ctx.tool.as_str();

    // Resolve or generate name
    let name = if let Some(n) = explicit_name {
        n.to_string()
    } else {
        instance_names::generate_unique_name(db)?
    };

    // Remote instances are relay mirrors. Starting them remotely is intentionally
    // unsupported because the useful remote lifecycle operations are launch/resume/kill.
    if let Ok(Some(ref existing)) = db.get_instance_full(&name) {
        if crate::instances::is_remote_instance(existing) {
            bail!(
                "Remote start is not supported for '{name}'. Start it on the owning device instead."
            );
        }
    }

    // Check if already exists and active (only for explicit names —
    // generate_unique_name creates a placeholder row we must skip past)
    if explicit_name.is_some() {
        if let Ok(Some(existing)) = db.get_instance_full(&name) {
            if existing.status != "stopped" {
                println!("hcom already started for {}", name);
                return Ok(0);
            }
        }
    }

    instance_binding::initialize_instance_in_position_file(
        db,
        &name,
        None, // session_id
        None, // parent_session_id
        None, // parent_name
        None, // agent_id
        None, // transcript_path
        Some(tool),
        false, // background
        None,  // tag
        None,  // wait_timeout
        None,  // subagent_timeout
        None,  // hints
        None,  // cwd_override
    );

    // Bind process if we have a process_id
    if let Some(ref process_id) = ctx.process_id {
        if let Err(e) = db.set_process_binding(process_id, "", &name) {
            eprintln!("[hcom] warn: set_process_binding failed for {name}: {e}");
        }
    }

    // Print bootstrap
    let hcom_config = HcomConfig::load(None).unwrap_or_else(|e| {
        eprintln!("[hcom] warn: config load failed, using defaults: {e}");
        let mut c = HcomConfig::default();
        c.normalize();
        c
    });

    let bootstrap_text = bootstrap::get_bootstrap(
        db,
        hcom_dir,
        &name,
        tool,
        false,
        ctx.is_launched,
        &ctx.notes,
        &hcom_config.tag,
        relay::is_relay_enabled(&hcom_config),
        None,
    );

    println!("[hcom:{}]", name);
    println!("{}", bootstrap_text);

    // Log
    db.log_event(
        "life",
        &name,
        &json!({
            "action": "started",
            "tool": tool,
            "name": name,
        }),
    )
    .ok();

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use rusqlite::params;
    use serde_json::json;
    use serial_test::serial;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn make_ctx(tool_env: &[(&str, &str)], cwd: &str) -> HcomContext {
        let mut env: HashMap<String, String> = std::env::vars().collect();
        for (k, v) in tool_env {
            env.insert((*k).to_string(), (*v).to_string());
        }
        HcomContext::from_env(&env, PathBuf::from(cwd))
    }

    fn log_stopped_snapshot(
        db: &HcomDb,
        name: &str,
        tool: &str,
        directory: &str,
        session_id: &str,
        last_event_id: i64,
    ) {
        db.log_event(
            "life",
            name,
            &json!({
                "action": "stopped",
                "snapshot": {
                    "tool": tool,
                    "directory": directory,
                    "session_id": session_id,
                    "last_event_id": last_event_id
                }
            }),
        )
        .unwrap();
    }
    #[test]
    fn test_start_args_bare() {
        let args = StartArgs::try_parse_from(["start"]).unwrap();
        assert!(args.orphan.is_none());
        assert!(args.as_name.is_none());
    }

    #[test]
    fn test_start_args_orphan() {
        let args = StartArgs::try_parse_from(["start", "--orphan", "1234"]).unwrap();
        assert_eq!(args.orphan, Some("1234".to_string()));
        assert!(args.as_name.is_none());
    }

    #[test]
    fn test_start_args_rebind() {
        let args = StartArgs::try_parse_from(["start", "--as", "luna"]).unwrap();
        assert!(args.orphan.is_none());
        assert_eq!(args.as_name, Some("luna".to_string()));
    }

    #[test]
    fn test_start_args_bare_as_errors() {
        let err = StartArgs::try_parse_from(["start", "--as"]);
        assert!(err.is_err());
    }

    #[test]
    fn test_start_args_bare_orphan_errors() {
        let err = StartArgs::try_parse_from(["start", "--orphan"]);
        assert!(err.is_err());
    }

    #[test]
    fn test_start_args_unknown_flag_errors() {
        let err = StartArgs::try_parse_from(["start", "--bogus"]);
        assert!(err.is_err());
    }

    #[test]
    #[serial]
    fn test_start_rejects_remote_instances() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, origin_device_id, created_at) VALUES (?1, ?2, ?3)",
                params![
                    "luna:ABCD",
                    "remote-device",
                    crate::shared::time::now_epoch_f64()
                ],
            )
            .unwrap();

        let flags = crate::router::GlobalFlags {
            name: Some("luna:ABCD".to_string()),
            go: false,
        };
        let err = run(&["start".to_string()], &flags).unwrap_err();
        assert!(
            err.to_string().contains("Remote start is not supported"),
            "unexpected error: {err}"
        );
    }

    #[test]
    #[serial]
    fn test_start_rebind_rejects_cross_tool_stopped_snapshot_hijack() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();

        log_stopped_snapshot(
            &db,
            "fama",
            "codex",
            "/tmp/dasha-code/.worktrees/layer1-basic-conversation-fixes",
            "sid-fama",
            42,
        );

        let ctx = make_ctx(
            &[("CLAUDECODE", "1")],
            "/tmp/hcom-gan-harness/.worktrees/bench-infra",
        );

        let err = start_rebind(&db, "fama", &ctx, None).unwrap_err();
        assert!(
            err.to_string().contains("Refusing to reclaim 'fama'"),
            "unexpected error: {err}"
        );

        assert!(db.get_instance_full("fama").unwrap().is_none());
        assert_eq!(db.get_session_binding("sid-fama").unwrap(), None);
    }

    #[test]
    #[serial]
    fn test_start_rebind_allows_matching_stopped_snapshot_reclaim() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();

        log_stopped_snapshot(
            &db,
            "nova",
            "claude",
            "/tmp/dasha-code/.worktrees/layer1-basic-conversation-fixes",
            "sid-nova",
            77,
        );

        let ctx = make_ctx(
            &[("CLAUDECODE", "1")],
            "/tmp/dasha-code/.worktrees/layer1-basic-conversation-fixes",
        );

        let exit_code = start_rebind(&db, "nova", &ctx, None).unwrap();
        assert_eq!(exit_code, 0);

        let inst = db.get_instance_full("nova").unwrap().unwrap();
        assert_eq!(inst.tool, "claude");
        assert_eq!(
            inst.directory,
            "/tmp/dasha-code/.worktrees/layer1-basic-conversation-fixes"
        );
        assert_eq!(inst.last_event_id, 77);
    }

    #[test]
    #[serial]
    fn test_start_rebind_rejects_cross_directory_stopped_snapshot_hijack() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();

        log_stopped_snapshot(
            &db,
            "mira",
            "claude",
            "/tmp/dasha-code/.worktrees/layer1-basic-conversation-fixes",
            "sid-mira",
            18,
        );

        let ctx = make_ctx(
            &[("CLAUDECODE", "1")],
            "/tmp/hcom-gan-harness/.worktrees/bench-infra",
        );

        let err = start_rebind(&db, "mira", &ctx, None).unwrap_err();
        assert!(
            err.to_string().contains("Refusing to reclaim 'mira'"),
            "unexpected error: {err}"
        );

        assert!(db.get_instance_full("mira").unwrap().is_none());
    }

    #[test]
    #[cfg(unix)]
    fn test_same_path_resolves_symlink_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let alias = dir.path().join("alias");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();

        assert!(same_path(
            real.to_string_lossy().as_ref(),
            alias.to_string_lossy().as_ref()
        ));
    }
}
