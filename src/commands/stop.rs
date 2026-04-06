//! `hcom stop` command — end hcom participation.
//!
//!
//! Supports: self-stop, named stop, multi-stop, `all`, `tag:<name>`.
//! Inside AI tools, destructive ops require `--go` flag.

use crate::db::HcomDb;
use crate::hooks::common::stop_instance;
use crate::identity;
use crate::instances::{
    get_full_name, is_remote_instance, is_subagent_instance, parse_running_tasks,
    resolve_display_name,
};
use crate::log::log_info;
use crate::shared::{CommandContext, SENDER, SenderKind, is_inside_ai_tool};

/// Parsed arguments for `hcom stop`.
#[derive(clap::Parser, Debug)]
#[command(name = "stop", about = "Stop hcom participation")]
pub struct StopArgs {
    /// Targets to stop (names, tag:X, or "all")
    pub targets: Vec<String>,
}

/// Resolve the initiator name for event logging.
fn resolve_initiator(
    db: &HcomDb,
    ctx: Option<&CommandContext>,
    explicit_name: Option<&str>,
) -> String {
    if let Some(c) = ctx {
        if let Some(ref id) = c.identity {
            if matches!(id.kind, SenderKind::Instance) {
                return id.name.clone();
            }
        }
    }
    if let Some(name) = explicit_name {
        return name.to_string();
    }
    match identity::resolve_identity(db, None, None, None, None, None, None) {
        Ok(id) => id.name,
        Err(_) => "cli".to_string(),
    }
}

/// Main entry point for `hcom stop` command.
///
/// Returns exit code (0 = success, 1 = error).
pub fn cmd_stop(db: &HcomDb, args: &StopArgs, ctx: Option<&CommandContext>) -> i32 {
    let explicit_name = ctx.and_then(|c| c.explicit_name.as_deref());

    let targets: Vec<&str> = args.targets.iter().map(|s| s.as_str()).collect();

    // Handle 'all' target
    if targets.contains(&"all") {
        if targets.len() > 1 {
            eprintln!("Error: 'all' cannot be combined with other targets");
            return 1;
        }

        // Only stop local instances
        let instances = match db.iter_instances_full() {
            Ok(rows) => rows
                .into_iter()
                .filter(|i| !is_remote_instance(i))
                .collect::<Vec<_>>(),
            Err(e) => {
                eprintln!("Error: {e}");
                return 1;
            }
        };

        if instances.is_empty() {
            println!("Nothing to stop");
            return 0;
        }

        // Confirmation gate: inside AI tools, require --go
        if is_inside_ai_tool() && !ctx.map(|c| c.go).unwrap_or(false) {
            print_stop_preview("ALL", "all", &instances);
            return 0;
        }

        let launcher = resolve_initiator(db, ctx, explicit_name);
        log_info(
            "lifecycle",
            "stop.all",
            &format!("count={} initiated_by={launcher}", instances.len()),
        );

        let mut stopped_names = Vec::new();
        let mut bg_logs = Vec::new();

        for inst in &instances {
            let display = get_full_name(inst);
            stop_instance(db, &inst.name, &launcher, "stop_all");
            stopped_names.push(display.clone());

            if inst.background != 0 && !inst.background_log_file.is_empty() {
                bg_logs.push((display, inst.background_log_file.clone()));
            }
        }

        if stopped_names.is_empty() {
            println!("Nothing to stop");
        } else {
            println!("Stopped: {}", stopped_names.join(", "));
            if !bg_logs.is_empty() {
                println!("\nHeadless logs:");
                for (name, log_file) in &bg_logs {
                    println!("  {name}: {log_file}");
                }
            }
        }
        return 0;
    }

    // Handle tag:name syntax
    if targets.len() == 1 && targets[0].starts_with("tag:") {
        let tag = &targets[0][4..];
        let tag_matches = match db.iter_instances_full() {
            Ok(rows) => rows
                .into_iter()
                .filter(|i| i.tag.as_deref() == Some(tag) && !is_remote_instance(i))
                .collect::<Vec<_>>(),
            Err(e) => {
                eprintln!("Error: {e}");
                return 1;
            }
        };

        if tag_matches.is_empty() {
            // Check orphans for this tag (already stopped but process may still be running)
            let orphans = crate::pidtrack::get_orphan_processes(&crate::paths::hcom_dir(), None);
            let tagged_orphans: Vec<_> = orphans.iter().filter(|o| o.tag == tag).collect();
            if !tagged_orphans.is_empty() {
                let names: Vec<_> = tagged_orphans
                    .iter()
                    .flat_map(|o| o.names.iter())
                    .cloned()
                    .collect();
                println!(
                    "No active agents with tag '{tag}' (already stopped: {})",
                    names.join(", ")
                );
                println!("Use 'hcom kill tag:{tag}' to terminate their processes.");
                return 0;
            }
            eprintln!("Error: No agents with tag '{tag}'");
            return 1;
        }

        // Confirmation gate
        if is_inside_ai_tool() && !ctx.map(|c| c.go).unwrap_or(false) {
            print_stop_preview(&format!("tag:{tag}"), &format!("tag:{tag}"), &tag_matches);
            return 0;
        }

        let launcher = resolve_initiator(db, ctx, explicit_name);
        log_info(
            "lifecycle",
            "stop.tag",
            &format!(
                "tag={tag} count={} initiated_by={launcher}",
                tag_matches.len()
            ),
        );

        let mut stopped_names = Vec::new();
        let mut bg_logs = Vec::new();

        for inst in &tag_matches {
            let display = get_full_name(inst);
            stop_instance(db, &inst.name, &launcher, "tag_stop");
            stopped_names.push(display.clone());
            if inst.background != 0 && !inst.background_log_file.is_empty() {
                bg_logs.push((display, inst.background_log_file.clone()));
            }
        }

        println!("Stopped tag:{tag}: {}", stopped_names.join(", "));
        if !bg_logs.is_empty() {
            println!("\nHeadless logs:");
            for (name, log_file) in &bg_logs {
                println!("  {name}: {log_file}");
            }
        }
        return 0;
    }

    // Handle multiple explicit targets
    if targets.len() > 1 {
        let mut instances_to_stop = Vec::new();
        let mut not_found = Vec::new();

        for t in &targets {
            if t.starts_with("tag:") {
                eprintln!("Error: Cannot mix tag: with other targets: {t}");
                return 1;
            }
            let resolved = resolve_display_name(db, t);
            let name = resolved.as_deref().unwrap_or(t);
            match db.get_instance_full(name) {
                Ok(Some(data)) => instances_to_stop.push(data),
                _ => {
                    not_found.push(t.to_string());
                }
            }
        }

        if !not_found.is_empty() {
            let plural = if not_found.len() > 1 { "s" } else { "" };
            eprintln!("Error: Agent{plural} not found: {}", not_found.join(", "));
            return 1;
        }

        // Confirmation gate
        if is_inside_ai_tool() && !ctx.map(|c| c.go).unwrap_or(false) {
            print_stop_preview("", &targets.join(" "), &instances_to_stop);
            return 0;
        }

        let launcher = resolve_initiator(db, ctx, explicit_name);
        let mut stopped_names = Vec::new();
        let mut bg_logs = Vec::new();

        for inst in &instances_to_stop {
            if is_remote_instance(inst) {
                println!("Skipping remote instance: {}", get_full_name(inst));
                continue;
            }
            let display = get_full_name(inst);
            stop_instance(db, &inst.name, &launcher, "multi_stop");
            stopped_names.push(display.clone());
            if inst.background != 0 && !inst.background_log_file.is_empty() {
                bg_logs.push((display, inst.background_log_file.clone()));
            }
        }

        if !stopped_names.is_empty() {
            println!("Stopped: {}", stopped_names.join(", "));
        }
        if !bg_logs.is_empty() {
            println!("\nHeadless logs:");
            for (name, log_file) in &bg_logs {
                println!("  {name}: {log_file}");
            }
        }
        return 0;
    }

    // Single target or self-stop
    let instance_name = if !targets.is_empty() {
        // Named target
        let target = targets[0];
        let resolved = resolve_display_name(db, target);
        resolved.unwrap_or_else(|| target.to_string())
    } else {
        // Self-stop: resolve identity
        let identity = if let Some(c) = ctx {
            if let Some(ref id) = c.identity {
                Some(id.clone())
            } else {
                identity::resolve_identity(db, explicit_name, None, None, None, None, None)
                    .ok()
            }
        } else {
            identity::resolve_identity(db, None, None, None, None, None, None).ok()
        };

        let name = match identity {
            Some(id) => id.name,
            None => {
                eprintln!(
                    "Error: Cannot determine identity\nUsage: hcom stop <name> | hcom stop all | run 'hcom stop' inside Claude/Gemini/Codex"
                );
                return 1;
            }
        };

        // Guard: block subagents from stopping their parent (in_subagent_context check)
        if let Ok(Some(inst_data)) = db.get_instance_full(&name) {
            let rt = parse_running_tasks(inst_data.running_tasks.as_deref());
            if rt.active {
                eprintln!("Error: Cannot run hcom stop from within a Task subagent");
                return 1;
            }
        }

        name
    };

    // Handle SENDER (not real instance)
    if instance_name == SENDER {
        eprintln!("Error: Cannot resolve identity - launch via 'hcom <n>' for stable identity");
        return 1;
    }

    // Lookup instance
    let position = match db.get_instance_full(&instance_name) {
        Ok(Some(data)) => data,
        _ => {
            eprintln!("Error: '{instance_name}' not found");
            return 1;
        }
    };

    // Remote instance — send control via relay
    if is_remote_instance(&position) {
        if instance_name.contains(':') {
            let (name, device_short_id) = instance_name.rsplit_once(':').unwrap();
            let config = crate::config::HcomConfig::load(None).unwrap_or_default();
            if crate::relay::control::send_control_ephemeral(&config, "stop", name, device_short_id)
            {
                println!("Stop sent to {instance_name}");
                return 0;
            } else {
                eprintln!("Error: Failed to send stop to {instance_name} - relay unavailable");
                return 1;
            }
        }
        eprintln!("Error: Cannot stop remote '{instance_name}' - missing device suffix");
        return 1;
    }

    let launcher = resolve_initiator(db, ctx, explicit_name);
    let is_external_stop = !targets.is_empty();
    let reason = if is_external_stop { "external" } else { "self" };

    let display = get_full_name(&position);
    log_info(
        "lifecycle",
        "stop.single",
        &format!("name={instance_name} reason={reason} initiated_by={launcher}"),
    );

    stop_instance(db, &instance_name, &launcher, reason);

    if is_subagent_instance(&position) {
        println!("Stopped hcom for subagent {display}.");
    } else {
        println!("Stopped hcom for {display}.");
    }

    if position.background != 0 && !position.background_log_file.is_empty() {
        println!("\nHeadless log: {}", position.background_log_file);
    }

    0
}

/// Print a stop preview for any scope (all, tag, or named targets).
fn print_stop_preview(scope: &str, cmd_suffix: &str, instances: &[crate::db::InstanceRow]) {
    let count = instances.len();
    let names: Vec<String> = instances.iter().map(get_full_name).collect();
    let headless = instances.iter().filter(|i| i.background != 0).count();
    let interactive = count - headless;
    let instance_list = if count <= 8 {
        names.join(", ")
    } else {
        format!("{} ... (+{} more)", names[..6].join(", "), count - 6)
    };

    println!("\n== STOP {scope} PREVIEW ==");
    println!(
        "This will stop {count} instance{}.\n",
        if count != 1 { "s" } else { "" }
    );
    println!("Instances to stop:\n  {instance_list}\n");
    println!("What happens:");
    println!(
        "  • Headless instances ({headless}): process killed (SIGTERM, then SIGKILL after 2s)"
    );
    println!("  • Interactive instances ({interactive}): notified via TCP (graceful)");
    println!("  • All: stopped event logged with snapshot, instance rows deleted");
    println!("  • Subagents: recursively stopped when parent stops\n");
    println!("Instance data preserved in events table (life.stopped with snapshot).\n");
    println!("Add --go flag and run again to proceed:");
    println!("  hcom --go stop {cmd_suffix}\n");
}
