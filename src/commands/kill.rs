//! Kill command: `hcom kill <name(s)|all|tag:X>`
//!
//!
//! Sends SIGTERM to process groups and optionally closes terminal panes.

use std::collections::HashSet;

use crate::db::HcomDb;
use crate::hooks::common::stop_instance;
use crate::identity;
use crate::instances;
use crate::log::log_info;
use crate::paths;
use crate::pidtrack;
use crate::router::GlobalFlags;
use crate::terminal;
use anyhow::{Result, bail};

/// Parsed arguments for `hcom kill`.
#[derive(clap::Parser, Debug)]
#[command(name = "kill", about = "Kill agent processes")]
pub struct KillArgs {
    /// Targets to kill (names, "all", or "tag:X")
    pub targets: Vec<String>,
}

pub struct KillTrackedResult {
    pub target: String,
    pub pid: u32,
    pub kill_result: terminal::KillResult,
    pub pane_closed: bool,
    pub preset_name: String,
    pub pane_id: String,
}

const EPERM_RECHECK_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

/// Resolve who initiated the kill
fn resolve_initiator(db: &HcomDb, explicit_name: Option<&str>) -> String {
    if let Some(name) = explicit_name {
        return name.to_string();
    }
    match identity::resolve_identity(db, None, None, None, None, None, None) {
        Ok(id) if matches!(id.kind, crate::shared::SenderKind::Instance) => id.name,
        _ => "cli".to_string(),
    }
}

fn normalize_kill_result(
    name: &str,
    pid: u32,
    result: terminal::KillResult,
) -> terminal::KillResult {
    if !matches!(result, terminal::KillResult::PermissionDenied) {
        return result;
    }

    let pid_str = pid.to_string();
    log_info(
        "kill",
        "kill.eperm",
        &format!(
            "kill(2) returned EPERM for name={} pid={}; checking if process already exited",
            name, pid
        ),
    );
    std::thread::sleep(EPERM_RECHECK_DELAY);
    if !pidtrack::is_alive(pid) {
        log_info(
            "kill",
            "kill.eperm_resolved",
            &format!("name={} pid={} resolved to already_dead", name, pid_str),
        );
        terminal::KillResult::AlreadyDead
    } else {
        terminal::KillResult::PermissionDenied
    }
}

pub fn kill_tracked_instance(
    db: &HcomDb,
    name: &str,
    initiator: &str,
) -> Result<KillTrackedResult, String> {
    let inst = db
        .get_instance_full(name)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("Agent '{}' not found", name))?;
    let pid = inst
        .pid
        .ok_or_else(|| format!("No tracked PID for '{}'", name))? as u32;
    let is_headless = inst.background != 0;
    let (result, pane_closed, preset_name, pane_id) =
        kill_instance(db, name, pid, &inst, is_headless);
    stop_instance(db, name, initiator, "killed");

    Ok(KillTrackedResult {
        target: name.to_string(),
        pid,
        kill_result: result,
        pane_closed,
        preset_name,
        pane_id,
    })
}

fn handle_remote_kill_response(name: &str, response: &serde_json::Value) -> Result<i32> {
    let result = &response["result"];
    let kill_result = result["kill_result"].as_str();

    // No kill_result means an RPC-level failure (e.g. timeout, protocol error).
    if kill_result.is_none() {
        crate::relay::control::require_successful_rpc_result(response.clone())
            .map_err(anyhow::Error::msg)?;
        bail!("Remote kill returned no kill_result");
    }
    let kill_result = kill_result.unwrap();

    let pid = result["pid"].as_u64().unwrap_or(0);
    let pane_closed = result["pane_closed"].as_bool().unwrap_or(false);
    let preset_name = result["preset_name"].as_str().unwrap_or("");
    let pane_id = result["pane_id"].as_str().unwrap_or("");
    let pane_info = pane_info_str(pane_closed, preset_name, pane_id);

    if kill_result == "permission_denied" {
        eprintln!(
            "Permission denied to kill process group {} for '{}'",
            pid, name
        );
        return Ok(1);
    }

    let lines = render_remote_kill_feedback(name, pid, kill_result, &pane_info)?;
    for line in lines {
        println!("{line}");
    }
    Ok(0)
}

fn render_remote_kill_feedback(
    name: &str,
    pid: u64,
    kill_result: &str,
    pane_info: &str,
) -> Result<Vec<String>> {
    match kill_result {
        "sent" => Ok(vec![
            format!(
                "Sent SIGTERM to process group {} for '{}'{}",
                pid, name, pane_info
            ),
            format!("  To resume: hcom r {}", name),
        ]),
        "already_dead" => Ok(vec![
            format!(
                "Process group {} not found for '{}' (already terminated){}",
                pid, name, pane_info
            ),
            format!("  To resume: hcom r {}", name),
        ]),
        other => bail!("Remote kill failed for {name}: unexpected kill_result {other}"),
    }
}

/// Run the kill command.
pub fn run(argv: &[String], flags: &GlobalFlags) -> Result<i32> {
    // Filter out global flags already consumed by the router
    let mut filtered = vec!["kill".to_string()];
    let mut skip_next = false;
    for arg in argv {
        if skip_next {
            skip_next = false;
            continue;
        }
        match arg.as_str() {
            "kill" | "--go" => continue,
            "--name" => {
                skip_next = true;
                continue;
            }
            _ => filtered.push(arg.clone()),
        }
    }

    use clap::Parser;
    let kill_args = match KillArgs::try_parse_from(&filtered) {
        Ok(a) => a,
        Err(e) => {
            e.print().ok();
            return Ok(if e.use_stderr() { 1 } else { 0 });
        }
    };

    let targets = kill_args.targets;
    if targets.is_empty() {
        eprintln!(
            "Error: no target specified\n\nUsage: kill <TARGET>...\n\nFor more information, try '--help'."
        );
        return Ok(1);
    }
    let explicit_name = flags.name.clone();

    let db = HcomDb::open()?;
    let hcom_dir = paths::hcom_dir();
    let initiator = resolve_initiator(&db, explicit_name.as_deref());

    // If any target is "all", just kill all
    if targets.iter().any(|t| t == "all") {
        return kill_all(&db, &hcom_dir, &initiator);
    }

    let mut worst_exit = 0;
    for target in &targets {
        let exit = if let Some(tag) = target.strip_prefix("tag:") {
            kill_by_tag(&db, &hcom_dir, tag, &initiator)?
        } else {
            kill_single(&db, &hcom_dir, target, &initiator)?
        };
        if exit > worst_exit {
            worst_exit = exit;
        }
    }
    Ok(worst_exit)
}

/// Format pane close info
fn pane_info_str(pane_closed: bool, preset_name: &str, pane_id: &str) -> String {
    if pane_closed {
        if !pane_id.is_empty() {
            format!(" (closed {} pane {})", preset_name, pane_id)
        } else if !preset_name.is_empty() {
            format!(" (closed {} pane)", preset_name)
        } else {
            String::new()
        }
    } else if !preset_name.is_empty()
        && crate::config::get_merged_preset(preset_name).is_some_and(|p| p.close.is_some())
    {
        if crate::terminal::is_zellij_preset(preset_name) {
            return " (zellij pane close unconfirmed)".to_string();
        }
        format!(" (pane close failed for {})", preset_name)
    } else {
        String::new()
    }
}

/// Kill all instances.
fn kill_all(db: &HcomDb, hcom_dir: &std::path::Path, initiator: &str) -> Result<i32> {
    let instances = db.iter_instances_full()?;
    let mut killed = 0;
    let mut failed = 0;

    // Collect active PIDs for orphan filtering
    let mut active_pids = HashSet::new();

    for inst in &instances {
        // Skip remote instances
        if inst.origin_device_id.is_some() {
            continue;
        }

        if let Some(pid) = inst.pid {
            active_pids.insert(pid as u32);
            let is_headless = inst.background != 0;
            let (result, pane_closed, preset_name, pane_id) =
                kill_instance(db, &inst.name, pid as u32, inst, is_headless);
            let pane_info = pane_info_str(pane_closed, &preset_name, &pane_id);
            match result {
                terminal::KillResult::Sent => {
                    println!(
                        "Sent SIGTERM to process group {} for '{}'{}",
                        pid, inst.name, pane_info
                    );
                    killed += 1;
                }
                terminal::KillResult::AlreadyDead => {
                    println!(
                        "Process group {} not found for '{}' (already terminated){}",
                        pid, inst.name, pane_info
                    );
                    killed += 1;
                }
                terminal::KillResult::PermissionDenied => {
                    eprintln!(
                        "Permission denied to kill process group {} for '{}'",
                        pid, inst.name
                    );
                    failed += 1;
                }
            }
            // Clean up instance
            stop_instance(db, &inst.name, initiator, "killed");
            println!("  To resume: hcom r {}", inst.name);
        } else {
            // No PID tracked — just clean up
            stop_instance(db, &inst.name, initiator, "killed");
        }
    }

    // Kill orphans too
    let orphans = pidtrack::get_orphan_processes(hcom_dir, Some(&active_pids));
    for orphan in &orphans {
        let (result, pane_closed) = terminal::kill_process(
            orphan.pid,
            &orphan.terminal_preset,
            &orphan.pane_id,
            &orphan.process_id,
            &orphan.kitty_listen_on,
            &orphan.terminal_id,
            &orphan.zellij_session_name,
        );
        let names = orphan.names.join(", ");
        let pane_info = pane_info_str(pane_closed, &orphan.terminal_preset, &orphan.pane_id);
        let label = if !names.is_empty() || !pane_info.is_empty() {
            format!(" ({}{})", names, pane_info)
        } else {
            String::new()
        };
        match result {
            terminal::KillResult::Sent => {
                println!(
                    "Sent SIGTERM to orphan process group {}{}",
                    orphan.pid, label
                );
                killed += 1;
            }
            terminal::KillResult::AlreadyDead => {
                println!(
                    "Orphan process group {} already terminated{}",
                    orphan.pid, label
                );
            }
            terminal::KillResult::PermissionDenied => {
                failed += 1;
            }
        }
        pidtrack::remove_pid(hcom_dir, orphan.pid);
    }

    if killed == 0 && failed == 0 {
        println!("No processes with tracked PIDs found");
    } else {
        if failed > 0 {
            println!("Killed {}, {} failed", killed, failed);
        } else {
            println!("Killed {}", killed);
        }
    }

    Ok(if failed > 0 { 1 } else { 0 })
}

/// Kill instances by tag.
fn kill_by_tag(db: &HcomDb, hcom_dir: &std::path::Path, tag: &str, initiator: &str) -> Result<i32> {
    let instances = db.iter_instances_full()?;
    let tagged: Vec<_> = instances
        .iter()
        .filter(|inst| inst.tag.as_deref() == Some(tag) && inst.origin_device_id.is_none())
        .collect();

    let mut killed = 0;
    let mut failed = 0;

    // Kill active instances with this tag
    for inst in &tagged {
        if let Some(pid) = inst.pid {
            let is_headless = inst.background != 0;
            let (result, pane_closed, preset_name, pane_id) =
                kill_instance(db, &inst.name, pid as u32, inst, is_headless);
            let pane_info = pane_info_str(pane_closed, &preset_name, &pane_id);
            match result {
                terminal::KillResult::Sent => {
                    println!(
                        "Sent SIGTERM to process group {} for '{}'{}",
                        pid, inst.name, pane_info
                    );
                    killed += 1;
                }
                terminal::KillResult::AlreadyDead => {
                    println!(
                        "Process group {} already terminated for '{}'",
                        pid, inst.name
                    );
                }
                terminal::KillResult::PermissionDenied => {
                    eprintln!(
                        "Permission denied to kill process group {} for '{}'",
                        pid, inst.name
                    );
                    failed += 1;
                }
            }
            stop_instance(db, &inst.name, initiator, "killed");
        } else {
            // No PID tracked — clean up DB entry
            println!("No tracked process for '{}', stopping instance.", inst.name);
            stop_instance(db, &inst.name, initiator, "killed");
        }
    }

    // Also kill orphan processes with this tag (stopped but still running)
    let active_pids: HashSet<u32> = tagged
        .iter()
        .filter_map(|i| i.pid.map(|p| p as u32))
        .collect();
    let orphans = pidtrack::get_orphan_processes(hcom_dir, Some(&active_pids));
    let tagged_orphans: Vec<_> = orphans.iter().filter(|o| o.tag == tag).collect();
    for orphan in &tagged_orphans {
        let names = orphan.names.join(", ");
        let (result, pane_closed) = terminal::kill_process(
            orphan.pid,
            &orphan.terminal_preset,
            &orphan.pane_id,
            &orphan.process_id,
            &orphan.kitty_listen_on,
            &orphan.terminal_id,
            &orphan.zellij_session_name,
        );
        let pane_info = pane_info_str(pane_closed, &orphan.terminal_preset, &orphan.pane_id);
        match result {
            terminal::KillResult::Sent => {
                println!(
                    "Sent SIGTERM to stopped process group {} for '{}'{}",
                    orphan.pid, names, pane_info
                );
                killed += 1;
            }
            terminal::KillResult::AlreadyDead => {
                println!(
                    "Process group {} already terminated for '{}'",
                    orphan.pid, names
                );
            }
            terminal::KillResult::PermissionDenied => {
                eprintln!("Permission denied to kill process group {}", orphan.pid);
                failed += 1;
            }
        }
        pidtrack::remove_pid(hcom_dir, orphan.pid);
    }

    if tagged.is_empty() && tagged_orphans.is_empty() {
        eprintln!("No agents with tag '{}'", tag);
        return Ok(1);
    }

    println!("Killed {} (tag:{})", killed, tag);
    Ok(if failed > 0 { 1 } else { 0 })
}

/// Kill a single instance by name.
fn kill_single(
    db: &HcomDb,
    hcom_dir: &std::path::Path,
    target: &str,
    initiator: &str,
) -> Result<i32> {
    // Resolve display name
    let name = instances::resolve_display_name(db, target).unwrap_or_else(|| target.to_string());

    let inst = match db.get_instance_full(&name)? {
        Some(inst) => inst,
        None => {
            // Check orphans
            let orphans = pidtrack::get_orphan_processes(hcom_dir, None);
            // Also match by PID number (TUI sends kill by PID for orphans)
            let target_pid = target.parse::<u32>().ok();
            if let Some(orphan) = orphans.iter().find(|o| {
                o.names.contains(&target.to_string())
                    || o.process_id == target
                    || target_pid == Some(o.pid)
            }) {
                let (result, pane_closed) = terminal::kill_process(
                    orphan.pid,
                    &orphan.terminal_preset,
                    &orphan.pane_id,
                    &orphan.process_id,
                    &orphan.kitty_listen_on,
                    &orphan.terminal_id,
                    &orphan.zellij_session_name,
                );
                let pane_info =
                    pane_info_str(pane_closed, &orphan.terminal_preset, &orphan.pane_id);
                match result {
                    terminal::KillResult::Sent => {
                        println!(
                            "Sent SIGTERM to process group {} for stopped instance '{}'{}",
                            orphan.pid, target, pane_info
                        );
                    }
                    terminal::KillResult::AlreadyDead => {
                        println!(
                            "Process group {} not found for '{}' (already terminated){}",
                            orphan.pid, target, pane_info
                        );
                    }
                    terminal::KillResult::PermissionDenied => {
                        eprintln!("Permission denied to kill process group {}", orphan.pid);
                        return Ok(1);
                    }
                }
                pidtrack::remove_pid(hcom_dir, orphan.pid);
                return Ok(0);
            }
            bail!("Agent '{}' not found", target);
        }
    };

    if inst.origin_device_id.is_some() {
        if let Some((base_name, device_short_id)) =
            crate::relay::control::split_device_suffix(&name)
        {
            let result = crate::relay::control::dispatch_remote_raw(
                db,
                device_short_id,
                Some(&name),
                "kill",
                &serde_json::json!({ "target": base_name }),
                crate::relay::control::RPC_DEFAULT_TIMEOUT,
            )
            .map_err(anyhow::Error::msg)?;
            return handle_remote_kill_response(&name, &result);
        }
        bail!("Cannot kill remote '{name}' - missing device suffix");
    }

    if inst.pid.is_none() {
        bail!(
            "No tracked PID for '{}' — use 'hcom stop {}' instead",
            name,
            name
        );
    }
    let kill_result = kill_tracked_instance(db, &name, initiator).map_err(anyhow::Error::msg)?;
    let pid = kill_result.pid;
    let pane_closed = kill_result.pane_closed;
    let preset_name = kill_result.preset_name;
    let pane_id = kill_result.pane_id;
    let result = kill_result.kill_result;

    let pane_info = pane_info_str(pane_closed, &preset_name, &pane_id);
    match result {
        terminal::KillResult::Sent => {
            println!(
                "Sent SIGTERM to process group {} for '{}'{}",
                pid, name, pane_info
            );
            println!("  To resume: hcom r {}", name);
            Ok(0)
        }
        terminal::KillResult::AlreadyDead => {
            println!(
                "Process group {} not found for '{}' (already terminated){}",
                pid, name, pane_info
            );
            println!("  To resume: hcom r {}", name);
            Ok(0)
        }
        terminal::KillResult::PermissionDenied => {
            eprintln!(
                "Permission denied to kill process group {} for '{}'",
                pid, name
            );
            Ok(1)
        }
    }
}

/// Kill a process and close its terminal pane.
/// Returns (KillResult, pane_closed, preset_name, pane_id).
fn kill_instance(
    _db: &HcomDb,
    name: &str,
    pid: u32,
    instance: &crate::db::InstanceRow,
    is_headless: bool,
) -> (terminal::KillResult, bool, String, String) {
    // Headless instances have no terminal pane — skip pane close
    if is_headless {
        let (result, pane_closed) = terminal::kill_process(pid, "", "", "", "", "", "");
        let result = normalize_kill_result(name, pid, result);
        log_info(
            "kill",
            "lifecycle.kill",
            &format!(
                "name={} pid={} result={:?} pane_closed={} headless=true",
                name, pid, result, pane_closed
            ),
        );
        return (result, pane_closed, String::new(), String::new());
    }

    let ti = terminal::resolve_terminal_info(
        instance.terminal_preset_effective.as_deref(),
        instance.launch_context.as_deref(),
    );

    let (result, pane_closed) = terminal::kill_process(
        pid,
        &ti.preset_name,
        &ti.pane_id,
        &ti.process_id,
        &ti.kitty_listen_on,
        &ti.terminal_id,
        &ti.zellij_session_name,
    );
    let result = normalize_kill_result(name, pid, result);

    log_info(
        "kill",
        "lifecycle.kill",
        &format!(
            "name={} pid={} result={:?} pane_closed={}",
            name, pid, result, pane_closed
        ),
    );

    (
        result,
        pane_closed,
        ti.preset_name.clone(),
        ti.pane_id.clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_kill_no_target_fails() {
        // Missing required target → clap error → exit code 1
        let flags = GlobalFlags::default();
        let argv = vec!["kill".to_string()];
        let result = run(&argv, &flags).unwrap();
        assert_eq!(result, 1);
    }

    #[test]
    fn test_kill_args_parse_single() {
        use clap::Parser;
        let args = KillArgs::try_parse_from(["kill", "myagent"]).unwrap();
        assert_eq!(args.targets, vec!["myagent"]);
    }

    #[test]
    fn test_kill_args_parse_multiple() {
        use clap::Parser;
        let args = KillArgs::try_parse_from(["kill", "nozu", "zelu"]).unwrap();
        assert_eq!(args.targets, vec!["nozu", "zelu"]);
    }

    #[test]
    fn test_kill_args_no_target_is_empty_vec() {
        use clap::Parser;
        let args = KillArgs::try_parse_from(["kill"]).unwrap();
        assert!(args.targets.is_empty());
    }

    #[test]
    fn test_handle_remote_kill_response_permission_denied_returns_nonzero() {
        let result = handle_remote_kill_response(
            "luna:ABCD",
            &json!({
                "result": {
                    "pid": 42,
                    "kill_result": "permission_denied",
                    "pane_closed": false,
                    "preset_name": "",
                    "pane_id": ""
                }
            }),
        )
        .unwrap();
        assert_eq!(result, 1);
    }

    #[test]
    fn test_handle_remote_kill_response_permission_denied_with_closed_pane_succeeds() {
        let result = handle_remote_kill_response(
            "luna:ABCD",
            &json!({
                "ok": true,
                "result": {
                    "pid": 42,
                    "kill_result": "already_dead",
                    "pane_closed": true,
                    "preset_name": "kitty",
                    "pane_id": "@1"
                }
            }),
        )
        .unwrap();
        assert_eq!(result, 0);
    }

    #[test]
    fn test_render_remote_kill_feedback_sent_matches_cli_contract() {
        let lines = render_remote_kill_feedback("luna:ABCD", 42, "sent", " (closed kitty pane @1)")
            .unwrap();
        assert_eq!(
            lines,
            vec![
                "Sent SIGTERM to process group 42 for 'luna:ABCD' (closed kitty pane @1)"
                    .to_string(),
                "  To resume: hcom r luna:ABCD".to_string(),
            ]
        );
    }

    #[test]
    fn test_render_remote_kill_feedback_already_dead_matches_cli_contract() {
        let lines = render_remote_kill_feedback("luna:ABCD", 42, "already_dead", "").unwrap();
        assert_eq!(
            lines,
            vec![
                "Process group 42 not found for 'luna:ABCD' (already terminated)".to_string(),
                "  To resume: hcom r luna:ABCD".to_string(),
            ]
        );
    }

    #[test]
    fn test_handle_remote_kill_response_unknown_result_errors() {
        let err = handle_remote_kill_response(
            "luna:ABCD",
            &json!({
                "result": {
                    "pid": 42,
                    "kill_result": "mystery",
                    "pane_closed": false,
                    "preset_name": "",
                    "pane_id": ""
                }
            }),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unexpected kill_result mystery"));
    }
}
