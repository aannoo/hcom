//! One-time tips shown on first use of commands.
//!
//! Uses kv store to track which tips have been shown per instance.

use crate::db::HcomDb;

pub struct LaunchTipsContext<'a> {
    pub launched: usize,
    pub tag: Option<&'a str>,
    pub launcher_name: Option<&'a str>,
    pub launcher_participating: bool,
    pub background: bool,
    pub terminal_mode: &'a str,
    pub terminal_auto_detected: bool,
}

/// Centralized tip text.
pub fn get_tip(key: &str) -> Option<&'static str> {
    match key {
        "list:status" => Some(
            "[tip] Statuses: \u{25b6} active (will read new msgs very soon)  \u{25c9} listening (will read new msgs in <1s)\
              \u{25a0} blocked (needs human user approval)  \u{25cb} inactive (dead)  \u{25e6} unknown (neutral)",
        ),
        "list:types" => Some(
            "[tip] Types: [CLAUDE] [GEMINI] [CODEX] [OPENCODE] [claude] full features, automatic msg delivery\
             | [AD-HOC] [gemini] [codex] limited",
        ),
        // Send-side
        "send:intent:request" => Some(
            "[tip] intent=request: You signaled you expect a response. You'll be auto-notified if they end their turn or stop without responding. Safe to move on.",
        ),
        "send:intent:inform" => Some("[tip] intent=inform: You signaled no response needed."),
        "send:intent:ack" => {
            Some("[tip] intent=ack: You acknowledged receipt. Recipient won't respond.")
        }
        // Recv-side
        "recv:intent:request" => Some("[tip] intent=request: Sender expects a response."),
        "recv:intent:inform" => Some("[tip] intent=inform: Sender doesn't expect a response."),
        "recv:intent:ack" => {
            Some("[tip] intent=ack: Sender confirmed receipt. No response needed.")
        }
        // @mention matching
        "mention:matching" => Some(
            "[tip] @targets: @api- matches all with tag 'api' | @luna matches prefix | underscore blocks: @luna won't match luna_sub_1",
        ),
        // Subscriptions
        "sub:created" => Some(
            "[tip] You'll be notified via hcom message when the next matching event occurs. Safe to end your turn.",
        ),
        _ => None,
    }
}

/// Check if instance has seen this tip before.
pub fn has_seen_tip(db: &HcomDb, instance_name: &str, command: &str) -> bool {
    if instance_name.is_empty() {
        return true;
    }
    let key = format!("tip:{instance_name}:{command}");
    db.kv_get(&key).ok().flatten().is_some()
}

/// Mark tip as seen for this instance.
pub fn mark_tip_seen(db: &HcomDb, instance_name: &str, command: &str) {
    if instance_name.is_empty() {
        return;
    }
    let key = format!("tip:{instance_name}:{command}");
    let _ = db.kv_set(&key, Some("1"));
}

/// Show one-time tip for command if not seen before.
pub fn maybe_show_tip(db: &HcomDb, instance_name: &str, command: &str, json_output: bool) {
    if json_output {
        return;
    }
    let text = match get_tip(command) {
        Some(t) => t,
        None => return,
    };
    if has_seen_tip(db, instance_name, command) {
        return;
    }
    mark_tip_seen(db, instance_name, command);
    println!("\n{text}");
}

/// Print contextual tips after launch. One-time tips tracked per launcher via kv.
pub fn print_launch_tips(db: &HcomDb, ctx: LaunchTipsContext<'_>) {
    if ctx.launched == 0 {
        return;
    }

    let inside_tool = crate::shared::context::HcomContext::from_os().is_inside_ai_tool();
    let mut tips: Vec<String> = Vec::new();

    /// Append tip if not yet seen by this launcher.
    /// When launcher_name is None (ad-hoc usage), always show — no tracking.
    fn once(db: &HcomDb, tips: &mut Vec<String>, tip_id: Option<&str>, key: &str, text: &str) {
        if let Some(id) = tip_id {
            if has_seen_tip(db, id, key) {
                return;
            }
            mark_tip_seen(db, id, key);
        }
        tips.push(text.to_string());
    }

    // Terminal-mode awareness — built-in presets with close support
    let has_close = ctx.terminal_mode == "kitty"
        || ctx.terminal_mode.starts_with("kitty-")
        || ctx.terminal_mode == "wezterm"
        || ctx.terminal_mode.starts_with("wezterm-")
        || ctx.terminal_mode.starts_with("tmux")
        || ctx.terminal_mode.starts_with("zellij")
        || ctx.terminal_mode == "waveterm"
        || {
            // Check user-defined presets in config.toml for close command
            let config_path = crate::paths::config_toml_path();
            crate::config::load_toml_presets(&config_path)
                .and_then(|presets| {
                    presets
                        .get(ctx.terminal_mode)
                        .and_then(|p| p.get("close"))
                        .map(|_| true)
                })
                .unwrap_or(false)
        };
    let is_tmux = ctx.terminal_mode.starts_with("tmux");

    let managed = if has_close { "managed" } else { "unmanaged" };
    let auto = if ctx.terminal_auto_detected {
        ", auto-detected"
    } else {
        ""
    };
    tips.push(format!(
        "[info] Terminal: {} ({managed}{auto})",
        ctx.terminal_mode
    ));

    // --- Always-shown (batch-specific) ---

    if let Some(t) = ctx.tag {
        tips.push(format!(
            "[tip] Tag prefix targets all agents with that tag: hcom send @{t}- <message>"
        ));
    }

    if inside_tool && ctx.launcher_participating {
        once(
            db,
            &mut tips,
            ctx.launcher_name,
            "launch:notify",
            "[tip] You'll be automatically notified when instances are launched & ready",
        );
    }

    // --- One-time (kv-tracked) ---

    if inside_tool {
        if !ctx.launcher_participating {
            once(
                db,
                &mut tips,
                ctx.launcher_name,
                "launch:start",
                "[tip] Run 'hcom start' to receive notifications/messages from instances",
            );
        }

        if has_close {
            once(
                db,
                &mut tips,
                ctx.launcher_name,
                "launch:kill",
                "[tip] Kill agents and close their panes: hcom kill <name1> <name2> ...",
            );
        }

        if !ctx.background {
            once(
                db,
                &mut tips,
                ctx.launcher_name,
                "launch:term",
                "[tip] View an agent's screen: hcom term <name> | Inject keystrokes: hcom term inject <name> [text] --enter",
            );
        }

        if is_tmux || ctx.background {
            once(
                db,
                &mut tips,
                ctx.launcher_name,
                "launch:sub-blocked",
                "[tip] Get notified when an agent needs approval: hcom events sub --blocked <name>",
            );
        } else {
            once(
                db,
                &mut tips,
                ctx.launcher_name,
                "launch:sub-idle",
                "[tip] Get notified when an agent goes idle: hcom events sub --idle <name>",
            );
        }

        once(
            db,
            &mut tips,
            ctx.launcher_name,
            "list:status",
            get_tip("list:status").unwrap_or(""),
        );
    } else {
        once(
            db,
            &mut tips,
            ctx.launcher_name,
            "launch:send",
            "[tip] Send a message to an agent: hcom send @<name> <message>",
        );
        once(
            db,
            &mut tips,
            ctx.launcher_name,
            "launch:list",
            "[tip] Check status: hcom list",
        );
    }

    if !tips.is_empty() {
        println!("\n{}", tips.join("\n"));
    }
}
