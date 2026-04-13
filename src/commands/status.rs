//! `hcom status` command — system health overview.
//!
//!
//! Shows: version, directory, config, tools, terminal, agents, relay, logs.

use std::path::Path;

use serde_json::json;

use crate::db::HcomDb;
use crate::shared::CommandContext;

/// Parsed arguments for `hcom status`.
#[derive(clap::Parser, Debug)]
#[command(name = "status", about = "System health overview")]
pub struct StatusArgs {
    /// JSON output
    #[arg(long)]
    pub json: bool,
    /// Show recent log entries
    #[arg(long)]
    pub logs: bool,
}

// ── Tool Detection ───────────────────────────────────────────────────────

/// Check if a binary is available in PATH.
fn is_in_path(name: &str) -> bool {
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .any(|dir| Path::new(dir).join(name).exists())
}

/// Check Claude hook installation.
fn check_claude_hooks() -> bool {
    let settings_path = dirs::home_dir()
        .map(|h| h.join(".claude/settings.json"))
        .unwrap_or_default();
    if let Ok(content) = std::fs::read_to_string(&settings_path) {
        content.contains("hcom") || content.contains("hook-comms")
    } else {
        false
    }
}

/// Check Gemini hook installation.
fn check_gemini_hooks() -> bool {
    let settings_path = dirs::home_dir()
        .map(|h| h.join(".gemini/settings.json"))
        .unwrap_or_default();
    if let Ok(content) = std::fs::read_to_string(&settings_path) {
        content.contains("hcom") || content.contains("hook-comms")
    } else {
        false
    }
}

/// Check Codex hook installation (native hooks.json + feature flag + rules file).
fn check_codex_hooks() -> bool {
    crate::hooks::codex::verify_codex_hooks_installed(true)
}

/// Check OpenCode plugin installation.
fn check_opencode_hooks() -> bool {
    let plugin_paths = [
        dirs::home_dir().map(|h| h.join(".config/opencode/plugins/hcom.ts")),
        dirs::config_dir().map(|h| h.join("opencode/plugins/hcom.ts")),
    ];
    plugin_paths
        .iter()
        .any(|p| p.as_ref().is_some_and(|p| p.exists()))
}

// ── Status Collection ────────────────────────────────────────────────────

struct ToolStatus {
    name: &'static str,
    installed: bool,
    hooks: bool,
}

impl ToolStatus {
    fn symbol(&self) -> &'static str {
        if self.installed && self.hooks {
            "✓"
        } else if self.installed {
            "~"
        } else {
            "✗"
        }
    }
}

fn get_tool_statuses() -> Vec<ToolStatus> {
    vec![
        ToolStatus {
            name: "Claude",
            installed: is_in_path("claude"),
            hooks: check_claude_hooks(),
        },
        ToolStatus {
            name: "Gemini",
            installed: is_in_path("gemini"),
            hooks: check_gemini_hooks(),
        },
        ToolStatus {
            name: "Codex",
            installed: is_in_path("codex"),
            hooks: check_codex_hooks(),
        },
        ToolStatus {
            name: "OpenCode",
            installed: is_in_path("opencode"),
            hooks: check_opencode_hooks(),
        },
    ]
}

struct AgentCounts {
    active: i64,
    listening: i64,
    blocked: i64,
    error: i64,
    launching: i64,
    inactive: i64,
    total: i64,
}

fn get_agent_counts(db: &HcomDb) -> AgentCounts {
    let mut c = AgentCounts {
        active: 0,
        listening: 0,
        blocked: 0,
        error: 0,
        launching: 0,
        inactive: 0,
        total: 0,
    };

    if let Ok(mut stmt) = db
        .conn()
        .prepare("SELECT status, COUNT(*) FROM instances GROUP BY status")
    {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        }) {
            for row in rows.flatten() {
                match row.0.as_str() {
                    s if s.starts_with("active") => c.active += row.1,
                    "listening" => c.listening += row.1,
                    s if s.starts_with("blocked") => c.blocked += row.1,
                    "error" => c.error += row.1,
                    "launching" => c.launching += row.1,
                    "inactive" => c.inactive += row.1,
                    _ => c.inactive += row.1,
                }
            }
        }
    }

    c.total = c.active + c.listening + c.blocked + c.error + c.launching + c.inactive;
    c
}

// ── Main Entry Point ─────────────────────────────────────────────────────

/// Main entry point for `hcom status` command.
pub fn cmd_status(db: &HcomDb, args: &StatusArgs, _ctx: Option<&CommandContext>) -> i32 {
    let json_mode = args.json;
    let show_logs = args.logs;

    let hcom_dir = crate::paths::hcom_dir();
    let dir_exists = hcom_dir.exists();
    let dir_writable = if dir_exists {
        let test_file = hcom_dir.join(".write_test");
        let writable = std::fs::write(&test_file, "").is_ok();
        let _ = std::fs::remove_file(&test_file);
        writable
    } else {
        false
    };

    let tools = get_tool_statuses();
    let counts = get_agent_counts(db);
    let dev_root = crate::router::resolve_effective_dev_root(db.path());

    // Check config validity
    let mut config_errors: Vec<String> = Vec::new();
    let config_valid = match std::fs::read_to_string(hcom_dir.join("config.toml")) {
        Ok(c) => match c.parse::<toml::Table>() {
            Ok(_) => true,
            Err(e) => {
                config_errors.push(e.to_string());
                false
            }
        },
        Err(_) => true, // No config file = valid (defaults)
    };

    // Terminal — read from config
    let config = crate::config::load_config_snapshot().core;
    let terminal_config = config.terminal.clone();
    let terminal_available = if terminal_config == "default"
        || terminal_config == "custom"
        || terminal_config == "print"
        || terminal_config.contains("{script}")
    {
        true
    } else {
        crate::config::is_known_terminal_preset_pub(&terminal_config)
    };

    // Relay — use proper status from relay module
    let relay = crate::relay::get_relay_status(&config, db);

    // Paths
    let hcom_dir_override = std::env::var("HCOM_DIR").is_ok();
    let project_root = crate::paths::get_project_root();

    // Settings paths
    let claude_settings_path = crate::hooks::claude::get_claude_settings_path();
    let gemini_settings_path = crate::hooks::gemini::get_gemini_settings_path();
    let codex_config_path = crate::hooks::codex::get_codex_config_path();

    if json_mode {
        let log_summary = crate::log::get_log_summary(1.0);
        // Call get_update_info once to avoid inconsistent state (it has side effects)
        let update_info = crate::update::get_update_info();
        let mut result = json!({
            "version": {
                "current": env!("CARGO_PKG_VERSION"),
                "latest": update_info.as_ref().map(|(v, _)| v.clone()),
                "update_available": update_info.is_some(),
                "update_cmd": update_info.as_ref().map(|(_, c)| *c),
            },
            "hcom_dir": hcom_dir.to_string_lossy(),
            "hcom_dir_override": hcom_dir_override,
            "hcom_exists": dir_exists,
            "hcom_writable": dir_writable,
            "project_root": project_root.to_string_lossy(),
            "config_valid": config_valid,
            "config_errors": config_errors,
            "tools": {
                "claude": {
                    "installed": tools[0].installed,
                    "hooks": tools[0].hooks,
                    "settings_path": claude_settings_path.to_string_lossy(),
                },
                "gemini": {
                    "installed": tools[1].installed,
                    "hooks": tools[1].hooks,
                    "settings_path": gemini_settings_path.to_string_lossy(),
                },
                "codex": {
                    "installed": tools[2].installed,
                    "hooks": tools[2].hooks,
                    "settings_path": codex_config_path.to_string_lossy(),
                },
                "opencode": {
                    "installed": tools[3].installed,
                    "hooks": tools[3].hooks,
                },
            },
            "terminal": {
                "config": terminal_config,
                "available": terminal_available,
            },
            "instances": {
                "active": counts.active,
                "listening": counts.listening,
                "blocked": counts.blocked,
                "error": counts.error,
                "launching": counts.launching,
                "inactive": counts.inactive,
                "total": counts.total,
            },
            "relay": {
                "configured": relay.configured,
                "enabled": relay.enabled,
                "status": relay.status,
                "error": relay.error,
                "last_push": relay.last_push,
                "broker": relay.broker,
            },
            "delivery": {},
            "logs": {
                "error_count": log_summary.get("error_count").and_then(|v| v.as_i64()).unwrap_or(0),
                "warn_count": log_summary.get("warn_count").and_then(|v| v.as_i64()).unwrap_or(0),
                "last_error": log_summary.get("last_error").cloned(),
                "entries": [],
            },
        });
        if let Some((path, source)) = &dev_root {
            result["dev_root"] = json!({
                "path": path.to_string_lossy(),
                "source": source,
                "binary": crate::shared::dev_root_binary(path)
                    .map(|p| p.to_string_lossy().into_owned()),
            });
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&result).unwrap_or_default()
        );
        return 0;
    }

    // Pretty output
    println!("hcom {}", env!("CARGO_PKG_VERSION"));
    println!();

    // Directory
    let dir_status = if dir_exists && dir_writable {
        "ok"
    } else if dir_exists {
        "read-only"
    } else {
        "missing"
    };
    println!("dir:       {} ({dir_status})", hcom_dir.display());
    if std::env::var("HCOM_DIR").is_ok() {
        println!(
            "           HCOM_DIR={}",
            std::env::var("HCOM_DIR").unwrap_or_default()
        );
    }

    // Config
    let config_symbol = if config_valid { "✓" } else { "✗" };
    let config_desc = if config_valid { "valid" } else { "invalid" };
    println!("config:    {config_symbol} {config_desc}");

    // Tools
    let tools_str: String = tools
        .iter()
        .map(|t| format!("{} {}", t.name, t.symbol()))
        .collect::<Vec<_>>()
        .join("  ");
    println!("tools:     {tools_str}");

    // Terminal — show preset name with availability
    if terminal_config == "default" {
        let detected = crate::terminal::detect_terminal_from_env();
        if let Some(ref name) = detected {
            println!("terminal:  default (auto: {name})");
        } else {
            let fallback = crate::terminal::get_default_fallback_terminal_name();
            println!("terminal:  default (fallback: {fallback})");
        }
    } else if terminal_config == "custom"
        || terminal_config == "print"
        || terminal_config.contains("{script}")
    {
        println!("terminal:  {terminal_config}");
    } else {
        let available = crate::config::is_known_terminal_preset_pub(&terminal_config);
        let sym = if available { "✓" } else { "✗" };
        println!("terminal:  {terminal_config} {sym}");
    }
    if let Some((path, source)) = &dev_root {
        println!("dev-root:  {} [{source}]", path.display());
    }

    println!(); // Blank line before instance section

    // Agents
    if counts.total == 0 {
        println!("agents:    none");
    } else {
        let mut parts = Vec::new();
        if counts.active > 0 {
            parts.push(format!("{} active", counts.active));
        }
        if counts.listening > 0 {
            parts.push(format!("{} listening", counts.listening));
        }
        if counts.blocked > 0 {
            parts.push(format!("{} blocked", counts.blocked));
        }
        if counts.inactive > 0 {
            parts.push(format!("{} inactive", counts.inactive));
        }
        println!("agents:    {}", parts.join(", "));
    }

    // Relay — show detailed status
    if relay.configured {
        if relay.enabled {
            match relay.status.as_deref() {
                Some("ok") => println!("relay:     connected"),
                Some("error") => {
                    let err = relay.error.as_deref().unwrap_or("unknown error");
                    println!("relay:     error ({err})");
                }
                _ => println!("relay:     enabled (not synced)"),
            }
        } else {
            println!("relay:     disabled");
        }
    } else {
        println!("relay:     not configured");
    }

    // Relay worker process status — only show when relay is enabled
    if relay.configured && relay.enabled {
        let relay_pid_path = crate::paths::hcom_dir().join(".tmp").join("relay.pid");
        let relay_running = if let Ok(pid_str) = std::fs::read_to_string(&relay_pid_path) {
            if let Ok(pid) = pid_str.trim().parse::<u32>() {
                crate::pidtrack::is_alive(pid)
            } else {
                false
            }
        } else {
            false
        };
        if relay_running {
            let pid_str = std::fs::read_to_string(&relay_pid_path)
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .map(|p| format!(" (PID {p})"))
                .unwrap_or_default();
            println!("relay-worker: running{pid_str}");
        } else {
            println!("relay-worker: not running");
        }
    }

    // Logs — always show summary; show recent entries when issues exist
    let log_summary = crate::log::get_log_summary(1.0);
    let error_count = log_summary
        .get("error_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let warn_count = log_summary
        .get("warn_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if error_count == 0 && warn_count == 0 {
        println!("logs:      \u{2713} ok");
    } else {
        let mut parts = Vec::new();
        if error_count > 0 {
            parts.push(format!(
                "{error_count} error{}",
                if error_count != 1 { "s" } else { "" }
            ));
        }
        if warn_count > 0 {
            parts.push(format!(
                "{warn_count} warn{}",
                if warn_count != 1 { "s" } else { "" }
            ));
        }
        let log_path = hcom_dir.join(".tmp/logs/hcom.log");
        if show_logs {
            println!("logs:      {} (1h)", parts.join(", "));
        } else {
            println!("logs:      {} (1h)  (hcom status --logs)", parts.join(", "));
        }
        println!("           {}", log_path.display());
        if show_logs {
            let entries = crate::log::get_recent_logs(1.0, &["ERROR", "WARN"], 20);
            for entry in &entries {
                let ts = entry.get("ts").and_then(|v| v.as_str()).unwrap_or("");
                let level = entry
                    .get("level")
                    .and_then(|v| v.as_str())
                    .unwrap_or("INFO");
                let subsystem = entry
                    .get("subsystem")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let event = entry.get("event").and_then(|v| v.as_str()).unwrap_or("");
                if level == "ERROR" || level == "WARN" {
                    let ts_short = if ts.len() > 8 {
                        &ts[ts.len() - 8..]
                    } else {
                        ts
                    };
                    println!("           {ts_short} [{level:<5}] {subsystem}.{event}");
                }
            }
        }
    }

    0
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::DEV_ROOT_KV_KEY;

    #[test]
    fn test_tool_symbol() {
        let t = ToolStatus {
            name: "Claude",
            installed: true,
            hooks: true,
        };
        assert_eq!(t.symbol(), "✓");

        let t = ToolStatus {
            name: "Claude",
            installed: true,
            hooks: false,
        };
        assert_eq!(t.symbol(), "~");

        let t = ToolStatus {
            name: "Claude",
            installed: false,
            hooks: false,
        };
        assert_eq!(t.symbol(), "✗");
    }

    #[test]
    fn test_is_in_path() {
        // ls should be in PATH on any Unix system
        assert!(is_in_path("ls"));
        assert!(!is_in_path("definitely_not_a_real_binary_xyz123"));
    }

    #[test]
    fn test_status_json_includes_dev_root_only_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::HcomDb::open_at(&dir.path().join("hcom.db")).unwrap();

        assert_eq!(db.kv_get(DEV_ROOT_KV_KEY).unwrap(), None);

        db.kv_set(DEV_ROOT_KV_KEY, Some("/tmp/dev-root")).unwrap();
        assert_eq!(
            crate::router::resolve_effective_dev_root(&dir.path().join("hcom.db")),
            Some((std::path::PathBuf::from("/tmp/dev-root"), "kv"))
        );
    }
}
