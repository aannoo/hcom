//! Start command: `hcom start [--name <agent-id>] [--as <name> [--relocate]] [--orphan <name|pid>]`
//!
//! Runs inside an already-running tool session rather than launching a new one.
//! Used for adhoc/manual setup, identity rebinding, and orphan recovery:
//! - Bare start: detect vanilla tool or create adhoc instance
//! - `--name <agent-id>`: register a subagent (a router-level global flag, not
//!   parsed by `StartArgs` — resolved in `run()` via `flags.name`)
//! - `--orphan`: recover orphaned PTY process
//! - `--as`: rebind session identity

use anyhow::{Result, bail};
use rusqlite::OptionalExtension;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;

use crate::bootstrap;
use crate::claude_actor;
use crate::config::HcomConfig;
use crate::db::{HcomDb, InstanceRow};
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
    /// Explicitly move a stopped top-level Codex identity to this task's directory
    #[arg(long)]
    pub relocate: bool,
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
    let relocate = start_args.relocate;

    if relocate && rebind_target.is_none() {
        bail!("--relocate requires --as <name>");
    }
    if relocate && orphan_target.is_some() {
        bail!("--relocate cannot be combined with --orphan");
    }

    let db = HcomDb::open()?;
    let hcom_dir = paths::hcom_dir();

    let ctx = HcomContext::from_os();
    let verified_actor = claude_actor::resolve_env_actor(&db).map_err(anyhow::Error::new)?;
    if let (Some(actor), Some(name)) = (verified_actor.as_ref(), flags.name.as_deref()) {
        claude_actor::ensure_explicit_matches(&db, actor, name).map_err(anyhow::Error::new)?;
    }

    let requested_name = flags
        .name
        .as_deref()
        .map(|name| identity::resolve_display_name(&db, name).unwrap_or_else(|| name.to_string()));

    // A verified child actor can only promote/use its existing row. It cannot
    // rebind or recover another identity, and it does not need --name.
    if let Some(actor) = verified_actor.as_ref()
        && let Some(actor_row) = db.get_instance_full(&actor.name)?
        && instances::is_subagent_instance(&actor_row)
    {
        if rebind_target.is_some() {
            println!("[HCOM] Subagents cannot use --as. End your turn.");
            return Ok(1);
        }
        if orphan_target.is_some() {
            println!("[HCOM] Subagents cannot use --orphan. End your turn.");
            return Ok(1);
        }
        return start_subagent(&db, &actor_row);
    }

    // Without a capability, retain the ordinary manual fallback. A direct
    // indexed child lookup supports the documented --name <agent-id> form
    // without scanning duplicated parent JSON.
    let subagent_via_name = if verified_actor.is_none() {
        requested_name
            .as_deref()
            .and_then(|id| detect_subagent(&db, id))
    } else {
        None
    };
    let subagent_via_as = if verified_actor.is_none() {
        rebind_target
            .as_deref()
            .and_then(|id| detect_subagent(&db, id))
    } else {
        None
    };

    if subagent_via_as.is_some() || (subagent_via_name.is_some() && rebind_target.is_some()) {
        println!("[HCOM] Subagents cannot change identity. End your turn.");
        return Ok(1);
    }

    if let Some(orphan) = orphan_target {
        return start_from_orphan(&db, &hcom_dir, &orphan, &ctx);
    }

    if let Some(rebind) = rebind_target {
        let current_name = verified_actor
            .as_ref()
            .map(|actor| actor.name.as_str())
            .or(requested_name.as_deref());
        return start_rebind_with_options(&db, &rebind, &ctx, current_name, relocate);
    }

    if let Some(subagent) = subagent_via_name {
        return start_subagent(&db, &subagent);
    }

    // A verified root actor stays the root even while children exist.
    let effective_name = verified_actor
        .as_ref()
        .map(|actor| actor.name.as_str())
        .or(requested_name.as_deref());
    start_bare(&db, &hcom_dir, &ctx, effective_name)
}

/// Resolve a live child row directly by agent_id (or by its exact row name).
fn detect_subagent(db: &HcomDb, check_id: &str) -> Option<InstanceRow> {
    let name = db
        .get_instance_by_agent_id(check_id)
        .ok()
        .flatten()
        .unwrap_or_else(|| check_id.to_string());
    let row = db.get_instance_full(&name).ok().flatten()?;
    row.parent_name.as_ref().filter(|name| !name.is_empty())?;
    Some(row)
}

/// Promote an existing dormant child row into active hcom participation.
fn start_subagent(db: &HcomDb, info: &InstanceRow) -> Result<i32> {
    let parent_name = info.parent_name.as_deref().unwrap_or("");
    if parent_name.is_empty() || info.agent_id.as_deref().unwrap_or("").is_empty() {
        bail!(
            "Subagent row '{}' is missing parent/agent identity",
            info.name
        );
    }

    let was_announced = info.name_announced != 0;
    lifecycle::set_status(db, &info.name, ST_ACTIVE, "tool:start", Default::default());
    instance_binding::capture_and_store_launch_context(db, &info.name);

    log_info(
        "lifecycle",
        "start.subagent",
        &format!(
            "name={} parent={} agent_id={} announced={}",
            info.name,
            parent_name,
            info.agent_id.as_deref().unwrap_or(""),
            was_announced
        ),
    );

    if was_announced {
        println!("hcom already started for {}", info.name);
        return Ok(0);
    }

    let bootstrap = bootstrap::get_subagent_bootstrap(&info.name, parent_name);
    if !bootstrap.is_empty() {
        println!("{bootstrap}");
    }
    let mut updates = serde_json::Map::new();
    updates.insert("name_announced".into(), serde_json::json!(true));
    instances::update_instance_position(db, &info.name, &updates);

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

#[derive(Debug, Clone)]
struct ChildLink {
    name: String,
    parent_name: Option<String>,
}

fn snapshot_child_links(db: &HcomDb, session_id: Option<&str>) -> Result<Vec<ChildLink>> {
    let Some(session_id) = session_id.filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    let mut stmt = db
        .conn()
        .prepare("SELECT name, parent_name FROM instances WHERE parent_session_id = ?")?;
    let rows = stmt.query_map(rusqlite::params![session_id], |row| {
        Ok(ChildLink {
            name: row.get(0)?,
            parent_name: row.get(1)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn restore_child_links_after_root_rebind(
    db: &HcomDb,
    links: &[ChildLink],
    session_id: &str,
    old_root: &str,
    new_root: &str,
) -> Result<()> {
    db.with_immediate_transaction(|txn| {
        for link in links {
            let parent_name = match link.parent_name.as_deref() {
                Some(parent) if parent == old_root => Some(new_root),
                other => other,
            };
            txn.execute(
                "UPDATE instances SET parent_session_id = ?, parent_name = ? WHERE name = ?",
                rusqlite::params![session_id, parent_name, &link.name],
            )?;
        }
        Ok(())
    })
}

/// Rebind session identity (`--as <name>`), preserving last_event_id and any
/// live Claude child hierarchy owned by the current root actor.
#[cfg(test)]
fn start_rebind(
    db: &HcomDb,
    rebind_target: &str,
    ctx: &HcomContext,
    explicit_name: Option<&str>,
) -> Result<i32> {
    start_rebind_with_options(db, rebind_target, ctx, explicit_name, false)
}

fn start_rebind_with_options(
    db: &HcomDb,
    rebind_target: &str,
    ctx: &HcomContext,
    explicit_name: Option<&str>,
    relocate: bool,
) -> Result<i32> {
    let hcom_dir = paths::hcom_dir();

    // Resolve the target name
    let target_name = identity::resolve_display_name_or_stopped(db, rebind_target)
        .unwrap_or_else(|| rebind_target.to_string());

    // Guard: refuse to reclaim a subagent slot. Subagents share their parent's
    // session_id, so `hcom start --as <subagent_name>` from inside a subagent
    // bash would rebind session_bindings[parent_sid] to the subagent name,
    // clobbering the parent's identity. `--as` is documented for top-level
    // restartable identities (compaction/resume/clear), not for subagent
    // lifecycle — which has its own SubagentStart bootstrap path.
    if db.was_subagent_name(&target_name) {
        eprintln!(
            "Error: '{target_name}' is a subagent slot; cannot be reclaimed with --as.\n\
             Subagents register via 'hcom start --name <agent-id>' in the SubagentStart context. If your session ended, stop working and end your turn."
        );
        return Ok(1);
    }

    let explicit_current_name = explicit_name.unwrap_or("");

    // Resolve session_id from process binding or existing instance
    let mut session_id: Option<String> = None;
    if let Some(ref process_id) = ctx.process_id
        && let Ok(Some((sid, _))) = db.get_process_binding_full(process_id)
    {
        session_id = sid.filter(|s| !s.is_empty());
    }
    if session_id.is_none()
        && !explicit_current_name.is_empty()
        && let Ok(Some(current_data)) = db.get_instance_full(explicit_current_name)
    {
        session_id = current_data.session_id.filter(|s| !s.is_empty());
    }
    if session_id.is_none() && ctx.tool == crate::tool::Tool::Claude {
        // A vanilla Claude session has neither a process binding nor, before its
        // first start, a row to read the id back from. Its own session id is
        // what makes the rebind stick: without it the reclaimed name stays
        // unbound and the identity it replaces is never cleaned up.
        session_id = resolve_claude_session_id(&ctx.raw_env);
    }
    if ctx.tool == crate::tool::Tool::Codex {
        // Codex Desktop is not launched through hcom, so it has no
        // HCOM_PROCESS_ID/process binding. Its thread id is the stable
        // session identity exposed to commands run inside the task.
        if let Some(codex_session_id) = resolve_codex_session_id(ctx) {
            if let Some(existing_session_id) = session_id.as_deref()
                && existing_session_id != codex_session_id
            {
                bail!(
                    "Refusing Codex rebind: current task session '{}' conflicts with existing session '{}'",
                    codex_session_id,
                    existing_session_id
                );
            }
            session_id = Some(codex_session_id);
        }
    }
    let current_name = if !explicit_current_name.is_empty() {
        explicit_current_name.to_string()
    } else if let Some(ref sid) = session_id {
        db.get_session_binding(sid)?.unwrap_or_default()
    } else {
        String::new()
    };
    let child_links = snapshot_child_links(db, session_id.as_deref())?;

    let relocation = if relocate {
        Some(validate_codex_relocation(
            db,
            &target_name,
            ctx,
            session_id.as_deref(),
            &current_name,
        )?)
    } else {
        None
    };
    let target_meta = if relocation.is_some() {
        None
    } else {
        load_rebind_target_metadata(db, &target_name).ok()
    };
    if let Some(ref meta) = target_meta {
        ensure_rebind_compatible(&target_name, meta, ctx, relocation.is_some())?;
    }

    // A relocation is deliberately not a generic rebind. A generic rebind
    // deletes the target before recreating it, which lets a registration that
    // wins between validation and mutation be deleted and stolen. Compare the
    // exact stopped proof, create the row, and bind the session under one
    // BEGIN IMMEDIATE transaction. Any competing winner makes the whole
    // transaction fail without changing that winner.
    if let Some(ref proof) = relocation {
        commit_codex_relocation(db, &target_name, ctx, proof)?;
        return finish_rebind_output(db, &hcom_dir, &target_name, ctx, &current_name, true);
    }

    // Preserve last_event_id from target (cursor preservation)
    let mut last_event_id = target_meta.as_ref().map(|m| m.last_event_id);
    let target_data = db.get_instance_full(&target_name)?;

    // Final fallback: use current max to avoid re-delivering old messages
    if last_event_id.is_none() {
        last_event_id = Some(db.get_last_event_id());
    }

    // Skip delete for remote instances (origin_device_id)
    if let Some(ref td) = target_data
        && (td.origin_device_id.is_none() || td.origin_device_id.as_deref() == Some(""))
        && let Err(e) = db.delete_instance(&target_name)
    {
        eprintln!("[hcom] warn: delete_instance failed for {target_name}: {e}");
    }

    // Clean up target's bindings
    if let Err(e) = db.delete_process_bindings_for_instance(&target_name) {
        eprintln!("[hcom] warn: delete_process_bindings failed for {target_name}: {e}");
    }
    if let Err(e) = db.delete_session_bindings_for_instance(&target_name) {
        eprintln!("[hcom] warn: delete_session_bindings failed for {target_name}: {e}");
    }

    // Delete old identity if different from target
    if !current_name.is_empty()
        && current_name != target_name
        && let Err(e) = db.delete_instance(&current_name)
    {
        eprintln!("[hcom] warn: delete_instance failed for {current_name}: {e}");
    }

    // Create fresh instance with the target name
    let tool = ctx.tool.as_str();
    let cwd_override = ctx.cwd.to_string_lossy().to_string();
    let transcript_path = relocation
        .as_ref()
        .map(|proof| proof.transcript_path.as_str());
    let initialized = instance_binding::initialize_instance_in_position_file(
        db,
        &target_name,
        session_id.as_deref(),
        None, // parent_session_id
        None, // parent_name
        None, // agent_id
        transcript_path,
        Some(tool),
        false, // background
        None,  // tag
        None,  // wait_timeout
        None,  // subagent_timeout
        None,  // hints
        Some(&cwd_override),
    );

    debug_assert!(relocation.is_none());
    if !initialized {
        bail!("Could not create identity '{target_name}'");
    }

    if let Some(ref sid) = session_id {
        let old_root = if current_name.is_empty() {
            target_name.as_str()
        } else {
            current_name.as_str()
        };
        restore_child_links_after_root_rebind(db, &child_links, sid, old_root, &target_name)?;
        if old_root != target_name {
            db.rebind_claude_root_actor_state(sid, old_root, &target_name)?;
        }
    }

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
        } else if ctx.tool == crate::tool::Tool::Claude
            && let Err(e) = db.mark_claude_session_validated(sid, &target_name)
        {
            // The cache still names the identity being replaced, and it is keyed
            // by session generation, so it does not expire on its own. Left
            // stale, every hook for this session resolves to no_instance: no
            // status, no delivery, and the reclaimed row is flagged
            // launch_failed ~30s later while the session is alive and bound.
            eprintln!("[hcom] warn: mark_claude_session_validated failed for {target_name}: {e}");
        }
    }
    if let Some(ref process_id) = ctx.process_id {
        let sid = session_id.as_deref().unwrap_or("");
        if let Err(e) = db.set_process_binding(process_id, sid, &target_name) {
            eprintln!("[hcom] warn: set_process_binding failed for {target_name}: {e}");
        }

        // Migrate notify endpoints before notify so wake reaches correct port
        if !current_name.is_empty()
            && current_name != target_name
            && let Err(e) = db.migrate_notify_endpoints(&current_name, &target_name)
        {
            eprintln!("[hcom] warn: migrate_notify_endpoints failed: {e}");
        }

        crate::notify::wake(db, &target_name, crate::notify::WakeKind::DELIVERY_LOOPS);
    }

    finish_rebind_output(db, &hcom_dir, &target_name, ctx, &current_name, false)
}

fn finish_rebind_output(
    db: &HcomDb,
    hcom_dir: &std::path::Path,
    target_name: &str,
    ctx: &HcomContext,
    current_name: &str,
    relocated: bool,
) -> Result<i32> {
    let tool = ctx.tool.as_str();
    let hcom_config = HcomConfig::load(None).unwrap_or_else(|_| {
        let mut c = HcomConfig::default();
        c.normalize();
        c
    });

    let bootstrap_text = bootstrap::get_bootstrap(
        db,
        hcom_dir,
        target_name,
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
    // Same reason as bare start: keep the new name visible in a tailed snapshot.
    println!("[hcom:{}]", target_name);

    log_info(
        "start",
        "rebind.complete",
        &format!(
            "from={} to={} relocated={}",
            current_name, target_name, relocated
        ),
    );

    Ok(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexRelocationProof {
    session_id: String,
    transcript_path: String,
    stopped_event_id: i64,
    stopped_event_data: String,
    last_event_id: i64,
}

const MAX_CODEX_SESSION_META_BYTES: u64 = 2 * 1024 * 1024;

fn validate_codex_relocation(
    db: &HcomDb,
    target_name: &str,
    ctx: &HcomContext,
    resolved_session_id: Option<&str>,
    current_name: &str,
) -> Result<CodexRelocationProof> {
    if ctx.tool != crate::tool::Tool::Codex {
        bail!("--relocate is supported only from the owning Codex Desktop task");
    }
    if db.get_instance_full(target_name)?.is_some() {
        bail!("Refusing to relocate '{target_name}': the identity is still live");
    }
    if !current_name.is_empty() && current_name != target_name {
        bail!(
            "Refusing to relocate '{target_name}': this task is already bound as '{current_name}'"
        );
    }
    if !ctx.cwd.is_absolute() || !ctx.cwd.is_dir() {
        bail!(
            "Refusing to relocate '{target_name}': current directory '{}' is not an existing absolute directory",
            ctx.cwd.display()
        );
    }

    let thread_id = resolve_codex_session_id(ctx)
        .ok_or_else(|| anyhow::anyhow!("Codex relocation requires CODEX_THREAD_ID"))?;
    let session_env = ctx
        .raw_env
        .get("CODEX_SESSION_ID")
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Codex relocation requires CODEX_SESSION_ID"))?;
    if thread_id != session_env {
        bail!(
            "Refusing Codex relocation: CODEX_THREAD_ID '{}' conflicts with CODEX_SESSION_ID '{}'",
            thread_id,
            session_env
        );
    }
    if resolved_session_id != Some(thread_id.as_str()) {
        bail!("Refusing Codex relocation: current session identity is not canonical");
    }
    if let Some(owner) = db.get_session_binding(&thread_id)? {
        bail!(
            "Refusing to relocate '{target_name}': session '{thread_id}' is already bound to '{owner}'"
        );
    }

    let (stopped_event_id, stopped_event_data): (i64, String) = db
        .conn()
        .query_row(
            "SELECT id, data FROM events
             WHERE type = 'life'
               AND instance = ?1
               AND json_extract(data, '$.action') = 'stopped'
             ORDER BY id DESC LIMIT 1",
            rusqlite::params![target_name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| anyhow::anyhow!("No stopped identity found for '{target_name}'"))?;
    let data: serde_json::Value = serde_json::from_str(&stopped_event_data)?;
    let snapshot = data
        .get("snapshot")
        .ok_or_else(|| anyhow::anyhow!("Stopped identity '{target_name}' has no snapshot"))?;

    if snapshot.get("tool").and_then(serde_json::Value::as_str) != Some("codex") {
        bail!("Refusing to relocate '{target_name}': stopped identity is not Codex");
    }
    if !json_identity_field_is_empty(snapshot.get("origin_device_id")) {
        bail!("Refusing to relocate '{target_name}': stopped identity is remote");
    }
    if !json_identity_field_is_empty(snapshot.get("parent_name"))
        || !json_identity_field_is_empty(snapshot.get("parent_session_id"))
        || !json_identity_field_is_empty(snapshot.get("agent_id"))
    {
        bail!("Refusing to relocate '{target_name}': stopped identity is a child task");
    }
    if snapshot.get("background").is_some_and(|value| {
        !value.is_null() && value.as_i64().is_none_or(|background| background != 0)
    }) {
        bail!("Refusing to relocate '{target_name}': stopped identity is not top-level");
    }
    if snapshot
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        != Some(thread_id.as_str())
    {
        bail!("Refusing to relocate '{target_name}': stopped session does not match this task");
    }

    let stopped_directory = snapshot
        .get("directory")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let current_directory = ctx.cwd.to_string_lossy();
    if stopped_directory.is_empty() || same_path(stopped_directory, &current_directory) {
        bail!(
            "Refusing to relocate '{target_name}': stopped and current directories are not distinct"
        );
    }

    let snapshot_transcript = snapshot
        .get("transcript_path")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Stopped identity '{target_name}' has no transcript"))?;
    let derived_transcript = crate::hooks::codex::derive_codex_transcript_path(&thread_id)
        .ok_or_else(|| anyhow::anyhow!("Could not locate the active Codex transcript"))?;
    if !same_path(snapshot_transcript, &derived_transcript) {
        bail!("Refusing to relocate '{target_name}': stopped and active transcript paths differ");
    }
    validate_codex_session_meta(&derived_transcript, &thread_id, stopped_directory)?;

    let last_event_id = snapshot
        .get("last_event_id")
        .and_then(serde_json::Value::as_i64)
        .filter(|value| *value >= 0)
        .ok_or_else(|| {
            anyhow::anyhow!("Stopped identity '{target_name}' has no valid event cursor")
        })?;

    Ok(CodexRelocationProof {
        session_id: thread_id,
        transcript_path: derived_transcript,
        stopped_event_id,
        stopped_event_data,
        last_event_id,
    })
}

fn json_identity_field_is_empty(value: Option<&serde_json::Value>) -> bool {
    match value {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::String(value)) => value.is_empty(),
        Some(_) => false,
    }
}

fn commit_codex_relocation(
    db: &HcomDb,
    target_name: &str,
    ctx: &HcomContext,
    proof: &CodexRelocationProof,
) -> Result<()> {
    let directory = ctx.cwd.to_string_lossy().to_string();
    let launch_context =
        crate::hooks::codex::directory_override_launch_context(&directory, &proof.session_id);
    let created_at = crate::shared::time::now_epoch_f64();
    let status_time = crate::shared::time::now_epoch_i64();
    let wait_timeout = HcomConfig::effective_timeout();

    db.with_immediate_transaction(|txn| {
        let latest_stop = txn
            .query_row(
                "SELECT id, data FROM events
                 WHERE type = 'life'
                   AND instance = ?1
                   AND json_extract(data, '$.action') = 'stopped'
                 ORDER BY id DESC LIMIT 1",
                rusqlite::params![target_name],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if latest_stop.as_ref()
            != Some(&(
                proof.stopped_event_id,
                proof.stopped_event_data.clone(),
            ))
        {
            bail!(
                "Refusing to relocate '{target_name}': stopped identity changed during verification"
            );
        }

        let target_exists: bool = txn.query_row(
            "SELECT EXISTS(SELECT 1 FROM instances WHERE name = ?1)",
            rusqlite::params![target_name],
            |row| row.get(0),
        )?;
        if target_exists {
            bail!(
                "Refusing to relocate '{target_name}': another task registered it during verification"
            );
        }

        let session_owner = txn
            .query_row(
                "SELECT instance_name FROM session_bindings WHERE session_id = ?1",
                rusqlite::params![&proof.session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(owner) = session_owner {
            bail!(
                "Refusing to relocate '{target_name}': session is already bound to '{owner}'"
            );
        }

        let target_binding = txn
            .query_row(
                "SELECT session_id FROM session_bindings WHERE instance_name = ?1 LIMIT 1",
                rusqlite::params![target_name],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if target_binding.is_some() {
            bail!(
                "Refusing to relocate '{target_name}': target binding changed during verification"
            );
        }

        let instance_session_owner = txn
            .query_row(
                "SELECT name FROM instances WHERE session_id = ?1 LIMIT 1",
                rusqlite::params![&proof.session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(owner) = instance_session_owner {
            bail!(
                "Refusing to relocate '{target_name}': session is already owned by '{owner}'"
            );
        }

        let process_collision: bool = txn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM process_bindings
                 WHERE instance_name = ?1 OR session_id = ?2
             )",
            rusqlite::params![target_name, &proof.session_id],
            |row| row.get(0),
        )?;
        if process_collision {
            bail!(
                "Refusing to relocate '{target_name}': a process binding changed during verification"
            );
        }

        let initial_event_id = proof.last_event_id;

        txn.execute(
            "INSERT INTO instances (
                 name, session_id, last_event_id, last_stop, status, status_time,
                 status_context, directory, created_at, transcript_path, tool,
                 background, wait_timeout, name_announced, launch_context
             ) VALUES (
                 ?1, ?2, ?3, 0, 'inactive', ?4,
                 'new', ?5, ?6, ?7, 'codex',
                 0, ?8, 1, ?9
             )",
            rusqlite::params![
                target_name,
                &proof.session_id,
                initial_event_id,
                status_time,
                &directory,
                created_at,
                &proof.transcript_path,
                wait_timeout,
                &launch_context,
            ],
        )?;
        txn.execute(
            "INSERT INTO session_bindings (session_id, instance_name, created_at)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![&proof.session_id, target_name, created_at],
        )?;

        let written: (Option<String>, String, String, String, i64, String) = txn.query_row(
            "SELECT session_id, tool, directory, transcript_path, last_event_id, launch_context
             FROM instances WHERE name = ?1",
            rusqlite::params![target_name],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )?;
        let binding: String = txn.query_row(
            "SELECT instance_name FROM session_bindings WHERE session_id = ?1",
            rusqlite::params![&proof.session_id],
            |row| row.get(0),
        )?;
        if written.0.as_deref() != Some(proof.session_id.as_str())
            || written.1 != "codex"
            || written.2 != directory
            || written.3 != proof.transcript_path
            || written.4 != initial_event_id
            || written.5 != launch_context
            || binding != target_name
        {
            bail!("Relocated Codex identity failed transactional verification");
        }
        Ok(())
    })?;

    let row = db
        .get_instance_full(target_name)?
        .ok_or_else(|| anyhow::anyhow!("Relocated Codex identity disappeared after commit"))?;
    let binding = db.get_session_binding(&proof.session_id)?;
    if row.tool != "codex"
        || row.session_id.as_deref() != Some(proof.session_id.as_str())
        || !same_path(&row.directory, &directory)
        || !same_path(&row.transcript_path, &proof.transcript_path)
        || binding.as_deref() != Some(target_name)
        || crate::hooks::codex::pinned_directory_from_launch_context(
            row.launch_context.as_deref(),
            &proof.session_id,
        )
        .is_none_or(|pinned| !same_path(&pinned, &directory))
    {
        bail!("Relocated Codex identity failed final bound-state verification");
    }

    let _ = db.log_event(
        "life",
        target_name,
        &serde_json::json!({
            "action": "created",
            "by": "explicit-start-relocate-v1",
            "is_hcom_launched": false,
            "is_subagent": false,
            "parent_name": "",
        }),
    );
    Ok(())
}

fn validate_codex_session_meta(
    transcript_path: &str,
    session_id: &str,
    stopped_directory: &str,
) -> Result<()> {
    let file = std::fs::File::open(transcript_path)?;
    let mut reader = BufReader::new(file).take(MAX_CODEX_SESSION_META_BYTES + 1);
    let mut first_line = String::new();
    let bytes = reader.read_line(&mut first_line)?;
    if bytes == 0 || bytes as u64 > MAX_CODEX_SESSION_META_BYTES {
        bail!("Codex transcript session metadata is missing or too large");
    }
    let meta: serde_json::Value = serde_json::from_str(first_line.trim_end())?;
    let payload = meta
        .get("payload")
        .ok_or_else(|| anyhow::anyhow!("Codex transcript has no session metadata payload"))?;
    if meta.get("type").and_then(serde_json::Value::as_str) != Some("session_meta")
        || payload.get("id").and_then(serde_json::Value::as_str) != Some(session_id)
        || payload
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            != Some(session_id)
        || payload
            .get("originator")
            .and_then(serde_json::Value::as_str)
            != Some("Codex Desktop")
        || payload.get("source").and_then(serde_json::Value::as_str) != Some("vscode")
        || payload
            .get("thread_source")
            .and_then(serde_json::Value::as_str)
            != Some("user")
        || payload
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .is_none_or(|cwd| !same_path(cwd, stopped_directory))
    {
        bail!("Codex transcript does not describe this top-level Desktop task");
    }
    for field in [
        "parent_thread_id",
        "parent_session_id",
        "source_thread_id",
        "forked_from",
    ] {
        if payload
            .get(field)
            .is_some_and(|value| !value.is_null() && value.as_str().is_none_or(|s| !s.is_empty()))
        {
            bail!("Codex transcript describes a child or forked task");
        }
    }
    Ok(())
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
    allow_directory_relocation: bool,
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
    if !allow_directory_relocation
        && !meta.directory.is_empty()
        && !same_path(&meta.directory, &current_dir)
    {
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
        if let Ok(data) = serde_json::from_str::<serde_json::Value>(data_str)
            && data.get("action").and_then(|v| v.as_str()) == Some("stopped")
            && let Some(snapshot) = data.get("snapshot")
        {
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

    bail!("No rebind metadata found for '{}'", name)
}

/// Resolve the Claude session id visible to a CLI invocation.
///
/// Two sources, in order:
/// - `HCOM_CLAUDE_UNIX_SESSION_ID`: hcom's own SessionStart hook appends this
///   export to `CLAUDE_ENV_FILE`, which Claude runs before each Bash command.
/// - `CLAUDE_CODE_SESSION_ID`: Claude sets this directly in every Bash and
///   PowerShell subprocess, and it matches the `session_id` hooks receive.
///
/// The env-file round trip is the fragile one: it needs `CLAUDE_ENV_FILE` to
/// exist and our SessionStart to have run in this session generation. Without
/// the second source, a session that misses it cannot be recognized on a repeat
/// `hcom start`, which then mints a SECOND identity — the first stays bound to
/// nothing and later reports as launch_failed. Both values are set by Claude
/// for the session running this command, so either one binds identity.
fn resolve_claude_session_id(env: &HashMap<String, String>) -> Option<String> {
    ["HCOM_CLAUDE_UNIX_SESSION_ID", "CLAUDE_CODE_SESSION_ID"]
        .into_iter()
        .find_map(|key| env.get(key).filter(|value| !value.is_empty()).cloned())
}

/// Resolve the stable Codex task identity exposed to commands inside Desktop.
fn resolve_codex_session_id(ctx: &HcomContext) -> Option<String> {
    ctx.codex_thread_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// Live local Claude instances in this directory that no session id points at.
///
/// These are the plausible earlier identities of a session that exposes no id
/// of its own — the only useful thing to say when hcom cannot recognize it.
fn unbound_claude_candidates(db: &HcomDb, ctx: &HcomContext, exclude: &str) -> Vec<String> {
    let cwd = ctx.cwd.to_string_lossy();
    let mut rows: Vec<InstanceRow> = db
        .iter_instances_full()
        .unwrap_or_default()
        .into_iter()
        .filter(|row| {
            row.tool == "claude"
                && row.status != "stopped"
                && row.name != exclude
                && row.directory == cwd
                && row.session_id.is_none()
                && row.parent_name.is_none()
                && !crate::instances::is_remote_instance(row)
        })
        .collect();
    rows.sort_by(|a, b| b.created_at.total_cmp(&a.created_at));
    rows.truncate(4);
    rows.into_iter().map(|row| row.name).collect()
}

/// Path C: Bare start — detect tool or create adhoc instance.
fn start_bare(
    db: &HcomDb,
    hcom_dir: &std::path::Path,
    ctx: &HcomContext,
    explicit_name: Option<&str>,
) -> Result<i32> {
    let explicit_name = explicit_name
        .map(|name| identity::resolve_display_name(db, name).unwrap_or_else(|| name.to_string()));
    let explicit_name = explicit_name.as_deref();

    // Skip vanilla detection if --name is provided with an existing instance
    let has_valid_identity = explicit_name
        .and_then(|n| db.get_instance_full(n).ok().flatten())
        .is_some();

    // Vanilla tool detection: auto-install hooks for unmanaged AI tools.
    // Identity is already canonical on HcomContext, so route every released
    // hook-bearing integration through the typed Tool hook adapter. This keeps
    // bare `hcom start` aligned with `hcom hooks add` as integrations evolve.
    if !has_valid_identity && ctx.detect_vanilla_tool().is_some() {
        let vanilla_tool = ctx.tool;
        if !vanilla_tool.hooks().is_empty() && !vanilla_tool.verify_hooks_installed(false) {
            println!("Installing {} hooks...", vanilla_tool.as_str());
            let include_perms = crate::config::load_config_snapshot().core.auto_approve;
            match vanilla_tool.try_setup_hooks(include_perms) {
                Ok(()) => {
                    println!(
                        "\nRestart {} to enable automatic message delivery.",
                        vanilla_tool.spec().label
                    );
                    println!("Then run: hcom start");
                }
                Err(error) if error.is_empty() => {
                    eprintln!(
                        "Failed to install hooks. Run: hcom hooks add {}",
                        vanilla_tool.as_str()
                    );
                }
                Err(error) => {
                    eprintln!(
                        "Failed to install {} hooks: {error}\nRun: hcom hooks add {}",
                        vanilla_tool.as_str(),
                        vanilla_tool.as_str()
                    );
                }
            }
            return Ok(1);
        }

        // Gemini: ensure hooksConfig.enabled is set (self-heal for v0.26.0+)
        if vanilla_tool == crate::tool::Tool::Gemini {
            let _ = crate::hooks::gemini::ensure_hooks_enabled();
        }
    }

    let tool = ctx.tool.as_str();
    let session_id = match ctx.tool {
        crate::tool::Tool::Claude => resolve_claude_session_id(&ctx.raw_env),
        crate::tool::Tool::Codex => resolve_codex_session_id(ctx),
        _ => None,
    };

    if explicit_name.is_none()
        && let Some(ref session_id) = session_id
        && let Some(bound_name) = db.get_session_binding(session_id)?
    {
        // Only hcom writes session bindings, so a row keyed by this session's
        // own id is trusted identity evidence. Heal Claude bindings created by
        // older versions before returning the existing row.
        if ctx.tool == crate::tool::Tool::Claude {
            db.mark_claude_session_validated(session_id, &bound_name)?;
        }
        println!("hcom already started for {bound_name}");
        return Ok(0);
    }

    // Resolve or generate name
    let name = if let Some(n) = explicit_name {
        n.to_string()
    } else {
        instance_names::generate_unique_name(db)?
    };

    // Remote instances are relay mirrors. Starting them remotely is intentionally
    // unsupported because the useful remote lifecycle operations are launch/resume/kill.
    if let Ok(Some(ref existing)) = db.get_instance_full(&name)
        && crate::instances::is_remote_instance(existing)
    {
        bail!("Remote start is not supported for '{name}'. Start it on the owning device instead.");
    }

    // Check if already exists and active (only for explicit names —
    // generate_unique_name creates a placeholder row we must skip past)
    if explicit_name.is_some()
        && let Ok(Some(existing)) = db.get_instance_full(&name)
        && existing.status != "stopped"
    {
        println!("hcom already started for {}", name);
        return Ok(0);
    }

    instance_binding::initialize_instance_in_position_file(
        db,
        &name,
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
        None,  // cwd_override
    );

    if let Some(ref session_id) = session_id {
        db.set_session_binding(session_id, &name)?;
        if ctx.tool == crate::tool::Tool::Claude {
            db.mark_claude_session_validated(session_id, &name)?;
        }
    }

    // Bind process if we have a process_id
    if let Some(ref process_id) = ctx.process_id
        && let Err(e) = db.set_process_binding(process_id, "", &name)
    {
        eprintln!("[hcom] warn: set_process_binding failed for {name}: {e}");
    }

    // Claude builds old enough to expose neither session id leave nothing to
    // recognize this session by, so a later `hcom start` here mints another
    // identity. Say what was just created and name the way back instead of
    // letting the duplicate appear silently.
    if explicit_name.is_none() && ctx.tool == crate::tool::Tool::Claude && session_id.is_none() {
        let candidates = unbound_claude_candidates(db, ctx, &name);
        eprintln!(
            "[hcom] warn: this Claude session exposes no session id, so it was registered \
             as a new identity '{name}'. If it already had one{}, reclaim it with \
             `hcom start --as <name>` and drop this one with `hcom kill {name}`.",
            if candidates.is_empty() {
                String::new()
            } else {
                format!(" (unbound here: {})", candidates.join(", "))
            }
        );
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
    // Repeated deliberately: the header above sits on top of a long bootstrap, so
    // `hcom start | tail -n` shows none of it. A caller that cannot see its own
    // name re-runs start, which is one way duplicate identities appear.
    println!("[hcom:{}]", name);

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

    /// Claude context carrying exactly one session-id source, so an ambient
    /// value from the shell running the tests cannot decide the outcome.
    fn make_claude_ctx(session: Option<(&str, &str)>, cwd: &str) -> HcomContext {
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.remove("HCOM_CLAUDE_UNIX_SESSION_ID");
        env.remove("CLAUDE_CODE_SESSION_ID");
        env.insert("CLAUDECODE".to_string(), "1".to_string());
        if let Some((key, value)) = session {
            env.insert(key.to_string(), value.to_string());
        }
        HcomContext::from_env(&env, PathBuf::from(cwd))
    }

    fn make_codex_ctx(thread_id: Option<&str>, cwd: &str) -> HcomContext {
        let mut env: HashMap<String, String> = std::env::vars().collect();
        env.remove("HCOM_PROCESS_ID");
        env.remove("HCOM_LAUNCHED");
        env.remove("HCOM_PTY_MODE");
        env.remove("CODEX_SANDBOX");
        match thread_id {
            Some(value) => {
                env.insert("CODEX_THREAD_ID".to_string(), value.to_string());
                env.insert("CODEX_SESSION_ID".to_string(), value.to_string());
            }
            None => {
                env.remove("CODEX_THREAD_ID");
                env.remove("CODEX_SESSION_ID");
            }
        }
        HcomContext::from_env(&env, PathBuf::from(cwd))
    }

    fn write_codex_session_meta(home: &std::path::Path, session_id: &str, cwd: &str) -> String {
        let codex_home = home.join(".codex");
        let sessions = codex_home
            .join("sessions")
            .join("2026")
            .join("09")
            .join("02");
        std::fs::create_dir_all(&sessions).unwrap();
        unsafe { std::env::set_var("CODEX_HOME", &codex_home) };
        let transcript = sessions.join(format!("rollout-test-{session_id}.jsonl"));
        let meta = serde_json::json!({
            "type": "session_meta",
            "payload": {
                "id": session_id,
                "session_id": session_id,
                "cwd": cwd,
                "originator": "Codex Desktop",
                "source": "vscode",
                "thread_source": "user"
            }
        });
        std::fs::write(&transcript, format!("{meta}\n")).unwrap();
        transcript.to_string_lossy().to_string()
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
        assert!(!args.relocate);
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
        assert!(!args.relocate);
    }

    #[test]
    fn test_start_args_explicit_relocation() {
        let args =
            StartArgs::try_parse_from(["start", "--as", "cultivation", "--relocate"]).unwrap();
        assert_eq!(args.as_name.as_deref(), Some("cultivation"));
        assert!(args.relocate);
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
    fn test_start_relocation_requires_as_and_rejects_orphan() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let flags = crate::router::GlobalFlags::default();
        let missing_as = run(&["start".into(), "--relocate".into()], &flags).unwrap_err();
        assert!(missing_as.to_string().contains("requires --as"));

        let conflict = run(
            &[
                "start".into(),
                "--as".into(),
                "cultivation".into(),
                "--relocate".into(),
                "--orphan".into(),
                "old".into(),
            ],
            &flags,
        )
        .unwrap_err();
        assert!(conflict.to_string().contains("cannot be combined"));
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
    fn test_vanilla_claude_start_immediately_binds_exported_session() {
        struct RestoreEnv(Option<std::ffi::OsString>);
        impl Drop for RestoreEnv {
            fn drop(&mut self) {
                unsafe {
                    match self.0.take() {
                        Some(value) => std::env::set_var("HCOM_CLAUDE_UNIX_SESSION_ID", value),
                        None => std::env::remove_var("HCOM_CLAUDE_UNIX_SESSION_ID"),
                    }
                }
            }
        }

        let (_dir, hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        assert!(crate::hooks::claude::setup_claude_hooks(false));

        let _restore = RestoreEnv(std::env::var_os("HCOM_CLAUDE_UNIX_SESSION_ID"));
        unsafe {
            std::env::set_var("HCOM_CLAUDE_UNIX_SESSION_ID", "sess-vanilla");
        }
        let ctx = make_ctx(&[("CLAUDECODE", "1")], "/tmp/project");

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        let name = db
            .get_session_binding("sess-vanilla")
            .unwrap()
            .expect("bare vanilla start must bind immediately");
        let row = db.get_instance_full(&name).unwrap().unwrap();
        assert_eq!(row.session_id.as_deref(), Some("sess-vanilla"));
        assert_eq!(row.tool, "claude");
        assert_eq!(
            db.get_validated_claude_session_owner("sess-vanilla")
                .unwrap()
                .as_deref(),
            Some(name.as_str()),
            "CLI-created Claude bindings must be immediately trusted by hooks"
        );

        let transcript = hcom_dir.join("vanilla.jsonl");
        std::fs::write(&transcript, "{\"sessionId\":\"sess-vanilla\"}\n").unwrap();
        let mut hook_ctx = ctx.clone();
        hook_ctx.process_id = None;
        let (resolved, _, _) = crate::hooks::common::init_hook_context(
            &db,
            &hook_ctx,
            "sess-vanilla",
            transcript.to_str().unwrap(),
        );
        assert_eq!(resolved.as_deref(), Some(name.as_str()));

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        assert_eq!(
            db.get_session_binding("sess-vanilla").unwrap().as_deref(),
            Some(name.as_str()),
            "repeated bare start must retain the existing vanilla identity"
        );
    }

    #[test]
    fn test_resolve_claude_session_id_sources() {
        let env = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect()
        };

        assert_eq!(
            resolve_claude_session_id(&env(&[
                ("HCOM_CLAUDE_UNIX_SESSION_ID", "hook-sess"),
                ("CLAUDE_CODE_SESSION_ID", "claude-sess"),
            ])),
            Some("hook-sess".to_string()),
            "our own export stays the first source"
        );
        assert_eq!(
            resolve_claude_session_id(&env(&[("CLAUDE_CODE_SESSION_ID", "claude-sess")])),
            Some("claude-sess".to_string()),
            "Claude's own Bash env carries identity when the env file cannot"
        );
        assert_eq!(
            resolve_claude_session_id(&env(&[
                ("HCOM_CLAUDE_UNIX_SESSION_ID", ""),
                ("CLAUDE_CODE_SESSION_ID", "claude-sess"),
            ])),
            Some("claude-sess".to_string()),
            "an empty export is not identity"
        );
        assert_eq!(resolve_claude_session_id(&env(&[])), None);
    }

    #[test]
    fn test_resolve_codex_session_id_trims_and_rejects_empty_values() {
        assert_eq!(
            resolve_codex_session_id(&make_codex_ctx(Some("  thread-1  "), "/tmp/project")),
            Some("thread-1".to_string())
        );
        assert_eq!(
            resolve_codex_session_id(&make_codex_ctx(Some("   "), "/tmp/project")),
            None
        );
        assert_eq!(
            resolve_codex_session_id(&make_codex_ctx(None, "/tmp/project")),
            None
        );
    }

    #[test]
    #[serial]
    fn test_bare_codex_start_binds_and_reuses_thread_id() {
        let (_dir, hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        assert!(crate::hooks::codex::setup_codex_hooks(false));
        let ctx = make_codex_ctx(Some("thread-bare-codex"), "/tmp/project");

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        let name = db
            .get_session_binding("thread-bare-codex")
            .unwrap()
            .expect("the first bare start must bind the Desktop task");
        let row = db.get_instance_full(&name).unwrap().unwrap();
        assert_eq!(row.tool, "codex");
        assert_eq!(row.session_id.as_deref(), Some("thread-bare-codex"));

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        assert_eq!(
            db.get_session_binding("thread-bare-codex")
                .unwrap()
                .as_deref(),
            Some(name.as_str())
        );
        let codex_rows: Vec<String> = db
            .iter_instances_full()
            .unwrap()
            .into_iter()
            .filter(|row| row.tool == "codex")
            .map(|row| row.name)
            .collect();
        assert_eq!(
            codex_rows,
            vec![name],
            "repeat start must not mint a duplicate"
        );
    }

    #[test]
    #[serial]
    fn test_vanilla_claude_start_reuses_claude_code_session_id() {
        let (_dir, hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        assert!(crate::hooks::claude::setup_claude_hooks(false));

        // No CLAUDE_ENV_FILE round trip, so HCOM_CLAUDE_UNIX_SESSION_ID never
        // arrives — the case that used to mint a second identity per start.
        let ctx = make_claude_ctx(
            Some(("CLAUDE_CODE_SESSION_ID", "sess-claude-env")),
            "/tmp/project",
        );

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        let name = db
            .get_session_binding("sess-claude-env")
            .unwrap()
            .expect("CLAUDE_CODE_SESSION_ID must bind identity");
        assert_eq!(
            db.get_validated_claude_session_owner("sess-claude-env")
                .unwrap()
                .as_deref(),
            Some(name.as_str()),
            "hooks must trust the binding the CLI just created"
        );

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        assert_eq!(
            db.get_session_binding("sess-claude-env")
                .unwrap()
                .as_deref(),
            Some(name.as_str()),
            "repeat start must return the first identity, not mint a second"
        );
        let claude_rows: Vec<String> = db
            .iter_instances_full()
            .unwrap()
            .into_iter()
            .filter(|row| row.tool == "claude")
            .map(|row| row.name)
            .collect();
        assert_eq!(claude_rows, vec![name], "exactly one identity per session");
    }

    #[test]
    #[serial]
    fn test_vanilla_claude_rebind_binds_session_and_drops_old_identity() {
        let (_dir, hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        assert!(crate::hooks::claude::setup_claude_hooks(false));

        let ctx = make_claude_ctx(
            Some(("CLAUDE_CODE_SESSION_ID", "sess-rebind")),
            "/tmp/project",
        );
        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        let first = db.get_session_binding("sess-rebind").unwrap().unwrap();

        assert_eq!(start_rebind(&db, "nova", &ctx, None).unwrap(), 0);
        assert_eq!(
            db.get_session_binding("sess-rebind").unwrap().as_deref(),
            Some("nova"),
            "a reclaimed name must own the session that reclaimed it"
        );
        assert!(
            db.get_instance_full(&first).unwrap().is_none(),
            "the identity being replaced must not be left behind"
        );
        assert_eq!(
            db.get_validated_claude_session_owner("sess-rebind")
                .unwrap()
                .as_deref(),
            Some("nova"),
            "hooks must resolve the reclaimed name, not reject the session"
        );

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        assert_eq!(
            db.get_session_binding("sess-rebind").unwrap().as_deref(),
            Some("nova"),
            "a start after the rebind returns the reclaimed identity"
        );
    }

    #[test]
    #[serial]
    fn test_codex_rebind_uses_thread_id_without_process_binding() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, tool, directory, status, created_at) VALUES (?1, 'codex', ?2, 'active', ?3)",
                params!["desktop-old", "/tmp/project", crate::shared::time::now_epoch_f64()],
            )
            .unwrap();
        let ctx = make_codex_ctx(Some("thread-desktop"), "/tmp/project");

        assert_eq!(start_rebind(&db, "desktop-old", &ctx, None).unwrap(), 0);
        let row = db.get_instance_full("desktop-old").unwrap().unwrap();
        assert_eq!(row.tool, "codex");
        assert_eq!(row.session_id.as_deref(), Some("thread-desktop"));
        assert_eq!(
            db.get_session_binding("thread-desktop").unwrap().as_deref(),
            Some("desktop-old")
        );
    }

    #[test]
    #[serial]
    fn test_codex_rebind_ignores_empty_thread_id() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances (name, tool, directory, status, created_at) VALUES (?1, 'codex', ?2, 'active', ?3)",
                params!["desktop-empty", "/tmp/project", crate::shared::time::now_epoch_f64()],
            )
            .unwrap();
        let ctx = make_codex_ctx(Some("   "), "/tmp/project");

        assert_eq!(start_rebind(&db, "desktop-empty", &ctx, None).unwrap(), 0);
        let row = db.get_instance_full("desktop-empty").unwrap().unwrap();
        assert!(row.session_id.is_none());
        assert!(db.get_session_binding("").unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_codex_rebind_rejects_conflicting_process_session_before_mutation() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, directory, status, created_at)
                 VALUES ('current-codex', 'thread-stale', 'codex', '/tmp/project', 'active', 1)",
                [],
            )
            .unwrap();
        db.set_process_binding("process-current", "thread-stale", "current-codex")
            .unwrap();
        log_stopped_snapshot(
            &db,
            "cultivation",
            "codex",
            "/tmp/project",
            "thread-current",
            41,
        );
        let mut ctx = make_codex_ctx(Some("thread-current"), "/tmp/project");
        ctx.process_id = Some("process-current".to_string());

        let err = start_rebind(&db, "cultivation", &ctx, None).unwrap_err();
        assert!(err.to_string().contains("conflicts with existing session"));
        assert!(db.get_instance_full("cultivation").unwrap().is_none());
        assert!(db.get_instance_full("current-codex").unwrap().is_some());
        assert_eq!(
            db.get_process_binding_full("process-current")
                .unwrap()
                .unwrap()
                .0
                .as_deref(),
            Some("thread-stale")
        );
    }

    #[test]
    #[serial]
    fn test_codex_rebind_rejects_conflicting_explicit_row_session_before_mutation() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, directory, status, created_at)
                 VALUES ('current-codex', 'thread-stale', 'codex', '/tmp/project', 'active', 1)",
                [],
            )
            .unwrap();
        log_stopped_snapshot(
            &db,
            "cultivation",
            "codex",
            "/tmp/project",
            "thread-current",
            42,
        );
        let ctx = make_codex_ctx(Some("thread-current"), "/tmp/project");

        let err = start_rebind(&db, "cultivation", &ctx, Some("current-codex")).unwrap_err();
        assert!(err.to_string().contains("conflicts with existing session"));
        assert!(db.get_instance_full("cultivation").unwrap().is_none());
        let current = db.get_instance_full("current-codex").unwrap().unwrap();
        assert_eq!(current.session_id.as_deref(), Some("thread-stale"));
    }

    #[test]
    #[serial]
    fn test_codex_cli_rebind_still_rejects_cross_directory_for_same_env_session() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        log_stopped_snapshot(
            &db,
            "cultivation",
            "codex",
            "/tmp/old-project",
            "thread-cultivation",
            43,
        );
        let ctx = make_codex_ctx(Some("thread-cultivation"), "/tmp/cultivation");

        let err = start_rebind(&db, "cultivation", &ctx, None).unwrap_err();
        assert!(
            err.to_string()
                .contains("Refusing to reclaim 'cultivation'")
        );
        assert!(db.get_instance_full("cultivation").unwrap().is_none());
        assert!(
            db.get_session_binding("thread-cultivation")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    #[serial]
    fn test_codex_explicit_relocation_uses_verified_transcript_and_persists_pin() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let session_id = "thread-cultivation-relocate";
        let old_directory = tmp.path().join("old-scaffold");
        let new_directory = tmp.path().join("cultivation");
        std::fs::create_dir_all(&old_directory).unwrap();
        std::fs::create_dir_all(&new_directory).unwrap();
        let old_directory = old_directory.to_string_lossy().to_string();
        let new_directory = new_directory.to_string_lossy().to_string();
        let transcript = write_codex_session_meta(&home, session_id, &old_directory);
        db.log_event(
            "life",
            "cultivation",
            &serde_json::json!({
                "action": "stopped",
                "snapshot": {
                    "tool": "codex",
                    "directory": old_directory,
                    "session_id": session_id,
                    "transcript_path": transcript,
                    "parent_name": null,
                    "parent_session_id": null,
                    "agent_id": null,
                    "origin_device_id": null,
                    "last_event_id": 73
                }
            }),
        )
        .unwrap();
        let ctx = make_codex_ctx(Some(session_id), &new_directory);

        assert_eq!(
            start_rebind_with_options(&db, "cultivation", &ctx, None, true).unwrap(),
            0
        );
        let row = db.get_instance_full("cultivation").unwrap().unwrap();
        assert_eq!(row.session_id.as_deref(), Some(session_id));
        assert_eq!(row.last_event_id, 73);
        assert!(same_path(&row.directory, &new_directory));
        assert!(same_path(&row.transcript_path, &transcript));
        assert_eq!(
            crate::hooks::codex::pinned_directory_from_launch_context(
                row.launch_context.as_deref(),
                session_id,
            )
            .as_deref(),
            Some(new_directory.as_str())
        );
        assert_eq!(
            db.get_session_binding(session_id).unwrap().as_deref(),
            Some("cultivation")
        );
    }

    #[test]
    #[serial]
    fn test_codex_relocation_binding_failure_rolls_back_instance() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let session_id = "thread-binding-rollback";
        let old_directory = tmp.path().join("old-scaffold");
        let new_directory = tmp.path().join("cultivation");
        std::fs::create_dir_all(&old_directory).unwrap();
        std::fs::create_dir_all(&new_directory).unwrap();
        let old_directory = old_directory.to_string_lossy().to_string();
        let new_directory = new_directory.to_string_lossy().to_string();
        let transcript = write_codex_session_meta(&home, session_id, &old_directory);
        db.log_event(
            "life",
            "cultivation",
            &json!({
                "action": "stopped",
                "snapshot": {
                    "tool": "codex",
                    "directory": old_directory,
                    "session_id": session_id,
                    "transcript_path": transcript,
                    "last_event_id": 31
                }
            }),
        )
        .unwrap();
        let ctx = make_codex_ctx(Some(session_id), &new_directory);
        let proof =
            validate_codex_relocation(&db, "cultivation", &ctx, Some(session_id), "").unwrap();
        db.conn()
            .execute_batch(
                "CREATE TRIGGER deny_relocation_binding
                 BEFORE INSERT ON session_bindings
                 BEGIN SELECT RAISE(ABORT, 'forced binding failure'); END;",
            )
            .unwrap();

        let error = commit_codex_relocation(&db, "cultivation", &ctx, &proof).unwrap_err();
        assert!(error.to_string().contains("forced binding failure"));
        assert!(db.get_instance_full("cultivation").unwrap().is_none());
        assert!(db.get_session_binding(session_id).unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_codex_relocation_preserves_target_winner_after_proof() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let session_id = "thread-target-race";
        let old_directory = tmp.path().join("old-scaffold");
        let new_directory = tmp.path().join("cultivation");
        let winner_directory = tmp.path().join("winner");
        std::fs::create_dir_all(&old_directory).unwrap();
        std::fs::create_dir_all(&new_directory).unwrap();
        std::fs::create_dir_all(&winner_directory).unwrap();
        let old_directory = old_directory.to_string_lossy().to_string();
        let new_directory = new_directory.to_string_lossy().to_string();
        let winner_directory = winner_directory.to_string_lossy().to_string();
        let transcript = write_codex_session_meta(&home, session_id, &old_directory);
        db.log_event(
            "life",
            "cultivation",
            &json!({
                "action": "stopped",
                "snapshot": {
                    "tool": "codex",
                    "directory": old_directory,
                    "session_id": session_id,
                    "transcript_path": transcript,
                    "last_event_id": 32
                }
            }),
        )
        .unwrap();
        let ctx = make_codex_ctx(Some(session_id), &new_directory);
        let proof =
            validate_codex_relocation(&db, "cultivation", &ctx, Some(session_id), "").unwrap();

        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, directory, status, created_at)
                 VALUES ('cultivation', 'thread-winner', 'codex', ?1, 'active', 1)",
                params![winner_directory],
            )
            .unwrap();
        db.set_session_binding("thread-winner", "cultivation")
            .unwrap();

        let error = commit_codex_relocation(&db, "cultivation", &ctx, &proof).unwrap_err();
        assert!(error.to_string().contains("another task registered"));
        let winner = db.get_instance_full("cultivation").unwrap().unwrap();
        assert_eq!(winner.session_id.as_deref(), Some("thread-winner"));
        assert_eq!(winner.directory, winner_directory);
        assert_eq!(
            db.get_session_binding("thread-winner").unwrap().as_deref(),
            Some("cultivation")
        );
        assert!(db.get_session_binding(session_id).unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_codex_relocation_preserves_session_winner_after_proof() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let session_id = "thread-session-race";
        let old_directory = tmp.path().join("old-scaffold");
        let new_directory = tmp.path().join("cultivation");
        std::fs::create_dir_all(&old_directory).unwrap();
        std::fs::create_dir_all(&new_directory).unwrap();
        let old_directory = old_directory.to_string_lossy().to_string();
        let new_directory = new_directory.to_string_lossy().to_string();
        let transcript = write_codex_session_meta(&home, session_id, &old_directory);
        db.log_event(
            "life",
            "cultivation",
            &json!({
                "action": "stopped",
                "snapshot": {
                    "tool": "codex",
                    "directory": old_directory,
                    "session_id": session_id,
                    "transcript_path": transcript,
                    "last_event_id": 33
                }
            }),
        )
        .unwrap();
        let ctx = make_codex_ctx(Some(session_id), &new_directory);
        let proof =
            validate_codex_relocation(&db, "cultivation", &ctx, Some(session_id), "").unwrap();

        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, directory, status, created_at)
                 VALUES ('session-winner', ?1, 'codex', ?2, 'active', 1)",
                params![session_id, new_directory],
            )
            .unwrap();
        db.set_session_binding(session_id, "session-winner")
            .unwrap();

        let error = commit_codex_relocation(&db, "cultivation", &ctx, &proof).unwrap_err();
        assert!(error.to_string().contains("session is already bound"));
        assert!(db.get_instance_full("cultivation").unwrap().is_none());
        assert_eq!(
            db.get_session_binding(session_id).unwrap().as_deref(),
            Some("session-winner")
        );
    }

    #[test]
    #[serial]
    fn test_codex_relocation_rejects_malformed_snapshot_identity_fields() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let cases = [
            ("origin_device_id", json!(7)),
            ("parent_name", json!({"unexpected": true})),
            ("agent_id", json!(["unexpected"])),
            ("background", json!(true)),
            ("background", json!(1)),
        ];

        for (index, (field, bad_value)) in cases.into_iter().enumerate() {
            let bad_value_display = bad_value.to_string();
            let name = format!("cultivation-{index}");
            let session_id = format!("thread-malformed-{index}");
            let old_directory = tmp.path().join(format!("old-{index}"));
            let new_directory = tmp.path().join(format!("new-{index}"));
            std::fs::create_dir_all(&old_directory).unwrap();
            std::fs::create_dir_all(&new_directory).unwrap();
            let old_directory = old_directory.to_string_lossy().to_string();
            let new_directory = new_directory.to_string_lossy().to_string();
            let transcript = write_codex_session_meta(&home, &session_id, &old_directory);
            let mut snapshot = json!({
                "tool": "codex",
                "directory": old_directory,
                "session_id": session_id,
                "transcript_path": transcript,
                "last_event_id": 34
            });
            snapshot
                .as_object_mut()
                .unwrap()
                .insert(field.to_string(), bad_value);
            db.log_event(
                "life",
                &name,
                &json!({"action": "stopped", "snapshot": snapshot}),
            )
            .unwrap();
            let ctx = make_codex_ctx(Some(&session_id), &new_directory);

            let error = validate_codex_relocation(&db, &name, &ctx, Some(session_id.as_str()), "")
                .unwrap_err();
            assert!(
                error.to_string().contains("stopped identity"),
                "{field}={bad_value_display} unexpectedly produced: {error}"
            );
            assert!(db.get_instance_full(&name).unwrap().is_none());
            assert!(db.get_session_binding(&session_id).unwrap().is_none());
        }
    }

    #[test]
    #[serial]
    fn test_codex_relocation_rejects_mismatched_session_env_without_mutation() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let session_id = "thread-owned";
        let old_directory = tmp.path().join("old-scaffold");
        let new_directory = tmp.path().join("cultivation");
        std::fs::create_dir_all(&old_directory).unwrap();
        std::fs::create_dir_all(&new_directory).unwrap();
        let old_directory = old_directory.to_string_lossy().to_string();
        let new_directory = new_directory.to_string_lossy().to_string();
        let transcript = write_codex_session_meta(&home, session_id, &old_directory);
        db.log_event(
            "life",
            "cultivation",
            &serde_json::json!({
                "action": "stopped",
                "snapshot": {
                    "tool": "codex",
                    "directory": old_directory,
                    "session_id": session_id,
                    "transcript_path": transcript,
                    "origin_device_id": null,
                    "last_event_id": 73
                }
            }),
        )
        .unwrap();
        let mut ctx = make_codex_ctx(Some(session_id), &new_directory);
        ctx.raw_env
            .insert("CODEX_SESSION_ID".into(), "thread-foreign".into());

        let error = start_rebind_with_options(&db, "cultivation", &ctx, None, true).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("conflicts with CODEX_SESSION_ID")
        );
        assert!(db.get_instance_full("cultivation").unwrap().is_none());
        assert!(db.get_session_binding(session_id).unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_codex_relocation_rejects_remote_snapshot_without_mutation() {
        let (tmp, _hcom_dir, home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        let session_id = "thread-remote";
        let old_directory = tmp.path().join("old-scaffold");
        let new_directory = tmp.path().join("cultivation");
        std::fs::create_dir_all(&old_directory).unwrap();
        std::fs::create_dir_all(&new_directory).unwrap();
        let old_directory = old_directory.to_string_lossy().to_string();
        let new_directory = new_directory.to_string_lossy().to_string();
        let transcript = write_codex_session_meta(&home, session_id, &old_directory);
        db.log_event(
            "life",
            "cultivation",
            &serde_json::json!({
                "action": "stopped",
                "snapshot": {
                    "tool": "codex",
                    "directory": old_directory,
                    "session_id": session_id,
                    "transcript_path": transcript,
                    "origin_device_id": "remote-device",
                    "last_event_id": 73
                }
            }),
        )
        .unwrap();
        let ctx = make_codex_ctx(Some(session_id), &new_directory);

        let error = start_rebind_with_options(&db, "cultivation", &ctx, None, true).unwrap_err();
        assert!(error.to_string().contains("stopped identity is remote"));
        assert!(db.get_instance_full("cultivation").unwrap().is_none());
        assert!(db.get_session_binding(session_id).unwrap().is_none());
    }

    #[test]
    #[serial]
    fn test_unidentifiable_claude_start_lists_unbound_candidates() {
        let (_dir, hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        assert!(crate::hooks::claude::setup_claude_hooks(false));

        let cwd = std::env::current_dir().unwrap();
        let ctx = make_claude_ctx(None, cwd.to_str().unwrap());

        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        let first = db
            .iter_instances_full()
            .unwrap()
            .into_iter()
            .find(|row| row.tool == "claude")
            .expect("first start creates an identity")
            .name;
        assert!(
            db.get_instance_full(&first)
                .unwrap()
                .unwrap()
                .session_id
                .is_none(),
            "a session with no id leaves the row unbound"
        );

        // Without any session id hcom still cannot recognize the session, so the
        // second start mints another identity — the warning names this one back.
        assert_eq!(start_bare(&db, &hcom_dir, &ctx, None).unwrap(), 0);
        let second = db
            .iter_instances_full()
            .unwrap()
            .into_iter()
            .find(|row| row.tool == "claude" && row.name != first)
            .expect("second start mints a second identity")
            .name;
        assert_eq!(
            unbound_claude_candidates(&db, &ctx, &second),
            vec![first],
            "the earlier unbound identity is the reclaim candidate"
        );
    }

    #[test]
    #[serial]
    fn test_root_rebind_preserves_child_hierarchy_and_actor_state() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();

        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, status, status_time, last_seen, created_at)
                 VALUES ('nova', 'sess-1', 'claude', 'active', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, parent_session_id, parent_name, agent_id, tool, status,
                  status_time, last_seen, created_at)
                 VALUES ('nova_task_1', 'sess-1', 'nova', 'agent-1', 'claude',
                         'active', 0, 0, 0)",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, parent_session_id, parent_name, agent_id, tool, status,
                  status_time, last_seen, created_at)
                 VALUES ('nova_task_2', 'sess-1', 'nova_task_1', 'agent-2', 'claude',
                         'active', 0, 0, 0)",
                [],
            )
            .unwrap();

        let token = db
            .issue_claude_actor_capability("sess-1", "tool-root", None, "nova")
            .unwrap();

        let links = snapshot_child_links(&db, Some("sess-1")).unwrap();
        assert_eq!(links.len(), 2);
        db.delete_instance("nova").unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, status, status_time, last_seen, created_at)
                 VALUES ('sol', 'sess-1', 'claude', 'active', 0, 0, 0)",
                [],
            )
            .unwrap();

        restore_child_links_after_root_rebind(&db, &links, "sess-1", "nova", "sol").unwrap();
        db.rebind_claude_root_actor_state("sess-1", "nova", "sol")
            .unwrap();

        let direct = db.get_instance_full("nova_task_1").unwrap().unwrap();
        assert_eq!(direct.parent_session_id.as_deref(), Some("sess-1"));
        assert_eq!(direct.parent_name.as_deref(), Some("sol"));
        let nested = db.get_instance_full("nova_task_2").unwrap().unwrap();
        assert_eq!(nested.parent_session_id.as_deref(), Some("sess-1"));
        assert_eq!(nested.parent_name.as_deref(), Some("nova_task_1"));
        assert_eq!(
            db.resolve_claude_actor_capability(&token, "sess-1")
                .unwrap(),
            Some("sol".to_string())
        );
    }

    #[test]
    #[serial]
    fn test_same_name_root_rebind_restores_child_session_links() {
        let (_dir, _hcom_dir, _home, _guard) = crate::hooks::test_helpers::isolated_test_env();
        let db = HcomDb::open().unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, session_id, tool, directory, status, status_time, last_seen, created_at)
                 VALUES ('nova', 'sess-1', 'claude', '/tmp/project', 'active', 0, 0, 1)",
                [],
            )
            .unwrap();
        db.set_session_binding("sess-1", "nova").unwrap();
        db.conn()
            .execute(
                "INSERT INTO instances
                 (name, parent_session_id, parent_name, agent_id, tool, status,
                  status_time, last_seen, created_at)
                 VALUES ('nova_task_1', 'sess-1', 'nova', 'agent-1', 'claude',
                         'active', 0, 0, 2)",
                [],
            )
            .unwrap();
        let token = db
            .issue_claude_actor_capability("sess-1", "tool-child", Some("agent-1"), "nova_task_1")
            .unwrap();

        let ctx = make_ctx(&[("CLAUDECODE", "1")], "/tmp/project");
        assert_eq!(start_rebind(&db, "nova", &ctx, Some("nova")).unwrap(), 0);

        let child = db.get_instance_full("nova_task_1").unwrap().unwrap();
        assert_eq!(child.parent_session_id.as_deref(), Some("sess-1"));
        assert_eq!(child.parent_name.as_deref(), Some("nova"));
        assert_eq!(
            db.resolve_claude_actor_capability(&token, "sess-1")
                .unwrap(),
            Some("nova_task_1".to_string())
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
