//! Build the one-time bootstrap prompt injected into a newly connected agent.
//!
//! Teaches the agent its identity and the `hcom` CLI contract (messaging,
//! listing, spawning). Concise for token efficiency; agents learn details via --help.

use std::collections::BTreeMap;
use std::fs;

use crate::db::HcomDb;
use crate::instances::get_full_name;
use crate::paths;
use crate::shared::constants::{SENDER, ST_ACTIVE, ST_LISTENING};

// Bundled script names (compile-time known).
// User scripts are discovered at runtime from ~/.hcom/scripts/.

// MAIN BOOTSTRAP TEMPLATE

const UNIVERSAL: &str = r#"[HCOM SESSION]
You have access to the hcom communication tool.
- Your name: {display_name}
- Authority: Prioritize @{SENDER} over others
- Important: Include this marker anywhere in your first response only: [hcom:{instance_name}]

You run hcom commands on behalf of the human user. The human uses natural language with you.

## MESSAGES

Response rules:
- From {SENDER} or intent=request → always respond
- intent=inform → respond only if useful
- intent=ack → don't respond

Routing rules:
- hcom message (<hcom> tags, hook feedback) → run `hcom send` to respond
- Normal user chat → respond in chat

## CAPABILITIES

You MUST use `hcom <cmd+flags> --name {instance_name}` for all hcom commands:

- Message: send @name(s) [--intent request|inform|ack] [--reply-to <id>] [--thread <thread_name>] -- "message text" 
  Or instead of --: --file <path> | --base64 <string> | pipe/heredoc
  Example: send @luna @nova --intent ack --reply-to 82 -- "ok"
- See who's active: list [-v] [--json] [--names] [--format '{{name}} {{status}}']
- Read another's conversation: transcript [name] [--range N-M] [--last N] [--full]
- View events: events [--last N] [--all] [--sql EXPR] [filters]
  Filters (same flag=OR, different=AND): --agent NAME | --type message|status|life | --status listening|active|blocked | --cmd PATTERN (contains, ^prefix, =exact) | --file PATH (*.py for glob, file.py for contains)
  Event-based notifications, watch agents, subscribe, react: events sub [filters] | --help
- Handoff context: bundle prepare
- Spawn agents: [num] <claude|gemini|codex|opencode> [--tag labelOrGroup] [--terminal tmux|kitty|wezterm|etc]
  Example: `hcom 1 claude --tag cool` -> automatic <hcom> msg when ready -> send it task via hcom send
  Resume: hcom r <name> [args] | Fork: hcom f <name> [args] | Kill: hcom kill <name(s)>
  background, set prompt, system, forward args: <claude|gemini|codex|opencode> --help
- Run workflows: run <script> [args] [--help]
  {scripts}
- View agent screen: term [name] | inject text/enter: term inject <name> ['text'] [--enter]
- Other commands: status (diagnostics), config (set terminal, etc), relay (remote)

If unsure about syntax, always run `hcom <command> --help` FIRST. Do not guess.

## RULES

1. Task via hcom → ack immediately, do work, report via hcom
2. No filler messages (greetings, thanks, congratulations).
3. Use --intent on sends: request (want reply), inform (dont need reply), ack (responding).
4. User says "the gemini/claude/codex agent" or unclear → run `hcom list` to resolve name

Agent names are 4-letter CVCV words. When user mentions one, they mean an agent.
{active_instances}

This is session context, not a task for immediate action."#;

const TAG_NOTICE: &str = r#"
You are tagged "{tag}". Message your group: send @{tag}- -- msg"#;

const RELAY_NOTICE: &str = r#"
Remote agents have suffix (e.g., `luna:BOXE`). @luna = local only; @luna:BOXE = remote. Remote event IDs 42:BOXE."#;

const HEADLESS_NOTICE: &str = r#"
Headless mode: No one sees your chat, only hcom messages. Communicate via hcom send."#;

const UVX_CMD_NOTICE: &str = r#"
Note: hcom command in this environment is `{hcom_cmd}`. Substitute in examples."#;

// Tool-specific delivery

const DELIVERY_AUTO: &str = r#"## DELIVERY

Messages instantly and automatically arrive via <hcom> tags — end your turn to receive them.

## WAITING RULES

1. Never use `sleep [sec]` instead use `hcom listen [sec]`
2. Only use `hcom listen` when you are waiting for something not related to hcom
- Waiting for hcom message → end your turn
- Waiting for agent progress → `hcom events sub`, subscribe, end your turn"#;

const DELIVERY_CODEX_HCOM_LAUNCHED: &str = r#"## DELIVERY

New message notifications instantly and automatically arrive via <hcom> tags - end your turn to receive them.
IMPORTANT: <hcom> tags = run `hcom listen 1 --name {instance_name}` IMMEDIATELY to read message.

## WAITING RULES

1. Never use `sleep`
2. If you need a pure time-based pause (not waiting on hcom activity):
- use `hcom listen [sec]` instead of `sleep [sec]`
3. If you are waiting on hcom activity, DO NOT use any sleep, listen, polling or blocking. instead:
- Waiting for hcom message → end your turn immediately
- Waiting for agent progress → `hcom events sub`, subscribe, end your turn immediately"#;

const DELIVERY_ADHOC: &str = r#"## DELIVERY

Messages do NOT arrive automatically.
- Check messages: `hcom listen 1`
- Wait for messages: `hcom listen [timeout]`
- Always substitute `sleep [timeout]` with `hcom listen [timeout]`

<critical_rule>
LISTENING REQUIREMENT:
- After sending hcom message expecting reply → `hcom listen --timeout 60 --name {instance_name}`
- After receiving a task via hcom → do the work, report, then enter CONNECTED MODE
- User says "stay connected" → enter CONNECTED MODE

CONNECTED MODE:
1. Run exactly one foreground blocking command:
  `hcom listen --name {instance_name} --timeout [large_num]`
2. When it returns, read/handle the output, then you MUST manually run `hcom listen` again.
3. You MUST repeat steps 1 and 2 until the user says stop.
- Do not wrap `hcom listen` in `while`, `watch`, `xargs`, tmux helpers, or background jobs.

WRONG: hcom listen & (background)
RIGHT: hcom listen --timeout [sec] (blocking)
</critical_rule>

You are now registered with hcom."#;

const CLAUDE_ONLY: &str = r#"## SUBAGENTS

Subagents can join hcom:
1. Run Task with background=true
2. Tell subagent: `use hcom`

Subagents get their own hcom context and a random name. DO NOT give them any specific hcom syntax.
Set keep-alive: `hcom config -i self subagent_timeout [SEC]`"#;

// SUBAGENT BOOTSTRAP

const SUBAGENT_BOOTSTRAP: &str = r#"[HCOM SESSION]
You're participating in the hcom multi-agent network.
- Your name: {subagent_name}
- Your parent: {parent_name}
- Use "--name {subagent_name}" for all hcom commands
- Announce to parent once: send @{parent_name} --intent inform -- "Connected as {subagent_name}"

Messages instantly auto-arrive via <hcom> tags — end your turn to receive them.

- For hcom message waiting: end your turn (do not run `hcom listen`).
- For non-hcom pause/yield, use `hcom listen` instead of `sleep`.

Response rules:
- From {SENDER} or intent=request → always respond
- intent=inform → respond only if useful
- intent=ack → don't respond

hcom message → respond via hcom send

Commands:
  {hcom_cmd} send @name(s) [--intent request|inform|ack] [--reply-to <id>] -- <"message"> (or --stdin, --file <path>, --base64 <string>)
  Example: {hcom_cmd} send @luna @nova --intent ack --reply-to 82 -- "ok"  |  Code/markdown: replace "ok" with --file <path>
  {hcom_cmd} list --name {subagent_name}
  {hcom_cmd} events --name {subagent_name}
  {hcom_cmd} <cmd> --help --name {subagent_name}

Rules:
- Task via hcom → ack, work, report
- Authority: @{SENDER} > others
- Use --intent on sends: request (want reply), inform (FYI), ack (responding)"#;

// HELPERS

/// Get concise list of active instances grouped by tool.
/// Returns empty string if no active instances, or "\nActive (snapshot): claude: a, b | codex: c".
fn get_active_instances(db: &HcomDb, exclude_name: &str) -> String {
    let instances = match db.iter_instances_full() {
        Ok(v) => v,
        Err(_) => return String::new(),
    };

    let now = crate::shared::time::now_epoch_f64();
    let cutoff = now - 60.0;

    // Collect names grouped by tool, preserving insertion order via BTreeMap
    let mut by_tool: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut count = 0;

    for inst in &instances {
        if count >= 8 {
            break;
        }
        if inst.name == exclude_name {
            continue;
        }

        let status_time = inst.status_time as f64;
        if inst.status == ST_ACTIVE || inst.status == ST_LISTENING || status_time >= cutoff {
            let tool = if inst.tool.is_empty() {
                "claude"
            } else {
                &inst.tool
            };
            by_tool
                .entry(tool.to_string())
                .or_default()
                .push(get_full_name(inst));
            count += 1;
        }
    }

    if by_tool.is_empty() {
        return String::new();
    }

    let parts: Vec<String> = by_tool
        .iter()
        .map(|(tool, names)| format!("{}: {}", tool, names.join(", ")))
        .collect();

    format!("\nActive (snapshot): {}", parts.join(" | "))
}

/// Get combined list of bundled + user scripts.
/// Returns empty string if none, or "Scripts: clone, debate, ...".
fn get_scripts(hcom_dir: &std::path::Path) -> String {
    let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    // Bundled scripts (compile-time known)
    for (name, _) in crate::scripts::SCRIPTS {
        names.insert(name.to_string());
    }

    // User scripts from ~/.hcom/scripts/
    let user_dir = hcom_dir.join(paths::SCRIPTS_DIR);
    if let Ok(entries) = fs::read_dir(&user_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            if !name.is_empty() && !name.starts_with('_') && (ext == "py" || ext == "sh") {
                names.insert(name.to_string());
            }
        }
    }

    if names.is_empty() {
        return String::new();
    }

    format!(
        "Scripts: {}",
        names.into_iter().collect::<Vec<_>>().join(", ")
    )
}

// CONTEXT BUILDER

/// All context needed to render bootstrap templates.
struct BootstrapContext {
    instance_name: String,
    display_name: String,
    tag: String,
    relay_enabled: bool,
    hcom_cmd: String,
    is_launched: bool,
    is_headless: bool,
    active_instances: String,
    scripts: String,
    notes: String,
}

/// Build context for template substitution.
#[allow(clippy::too_many_arguments)]
fn build_context(
    db: &HcomDb,
    hcom_dir: &std::path::Path,
    instance_name: &str,
    _tool: &str,
    headless: bool,
    is_launched: bool,
    notes: &str,
    tag: &str,
    relay_enabled: bool,
    background_name: Option<&str>,
) -> BootstrapContext {
    // Load instance data for display name + tag override
    let instance_data = db
        .iter_instances_full()
        .ok()
        .and_then(|instances| instances.into_iter().find(|i| i.name == instance_name));

    let display_name = instance_data
        .as_ref()
        .map(get_full_name)
        .unwrap_or_else(|| instance_name.to_string());

    // Tag: instance-level overrides config-level
    let effective_tag = instance_data
        .as_ref()
        .and_then(|d| d.tag.as_deref())
        .filter(|t| !t.is_empty())
        .unwrap_or(tag)
        .to_string();

    let is_headless = headless || background_name.is_some();

    BootstrapContext {
        instance_name: instance_name.to_string(),
        display_name,
        tag: effective_tag,
        relay_enabled,
        hcom_cmd: crate::runtime_env::build_hcom_command(),
        is_launched,
        is_headless,
        active_instances: get_active_instances(db, instance_name),
        scripts: get_scripts(hcom_dir),
        notes: notes.to_string(),
    }
}

/// Apply string substitutions on template text.
/// Replaces {key} patterns with context values, then unescapes {{ → { and }} → }
/// then unescapes {{ → { and }} → } (template uses {{name}} to produce literal {name}).
fn render_template(template: &str, ctx: &BootstrapContext) -> String {
    template
        .replace("{display_name}", &ctx.display_name)
        .replace("{instance_name}", &ctx.instance_name)
        .replace("{SENDER}", SENDER)
        .replace("{tag}", &ctx.tag)
        .replace("{hcom_cmd}", &ctx.hcom_cmd)
        .replace("{active_instances}", &ctx.active_instances)
        .replace("{scripts}", &ctx.scripts)
        .replace("{{", "{")
        .replace("}}", "}")
}

// PUBLIC API

/// Build bootstrap text for an instance.
///
/// Args:
///   db: Database handle for reading active instances and instance data
///   hcom_dir: Path to hcom data directory (for scripts discovery)
///   instance_name: The instance name (as stored in DB)
///   tool: "claude", "gemini", "codex", "opencode", or "adhoc"
///   headless: Whether running in headless/background mode
///   is_launched: Whether instance was launched by hcom
///   notes: User notes (from HCOM_NOTES env var or config)
///   tag: Tag from config (instance-level tag overrides this)
///   relay_enabled: Whether relay is configured and enabled
///   background_name: Background log name (if headless via HCOM_BACKGROUND)
#[allow(clippy::too_many_arguments)]
pub fn get_bootstrap(
    db: &HcomDb,
    hcom_dir: &std::path::Path,
    instance_name: &str,
    tool: &str,
    headless: bool,
    is_launched: bool,
    notes: &str,
    tag: &str,
    relay_enabled: bool,
    background_name: Option<&str>,
) -> String {
    let ctx = build_context(
        db,
        hcom_dir,
        instance_name,
        tool,
        headless,
        is_launched,
        notes,
        tag,
        relay_enabled,
        background_name,
    );

    let mut parts: Vec<&str> = vec![UNIVERSAL];

    // Conditional sections
    if !ctx.tag.is_empty() {
        parts.push(TAG_NOTICE);
    }
    if ctx.relay_enabled {
        parts.push(RELAY_NOTICE);
    }
    if ctx.is_headless {
        parts.push(HEADLESS_NOTICE);
    }
    if ctx.hcom_cmd != "hcom" {
        parts.push(UVX_CMD_NOTICE);
    }

    // Tool-specific delivery
    if tool == "claude" || tool == "opencode" || (tool == "gemini" && ctx.is_launched) {
        parts.push(DELIVERY_AUTO);
    } else if tool == "codex" && ctx.is_launched {
        parts.push(DELIVERY_CODEX_HCOM_LAUNCHED);
    } else {
        parts.push(DELIVERY_ADHOC);
    }

    // Claude subagent info
    if tool == "claude" {
        parts.push(CLAUDE_ONLY);
    }

    // Join and substitute
    let joined = parts
        .iter()
        .map(|p| p.trim_matches('\n'))
        .collect::<Vec<_>>()
        .join("\n\n");

    let mut result = render_template(&joined, &ctx);

    // User notes (appended after render to avoid brace issues in user text)
    if !ctx.notes.is_empty() {
        result.push_str(&format!("\n\n## NOTES\n\n{}\n", ctx.notes));
    }

    // Rewrite hcom references if using alternate command
    if ctx.hcom_cmd != "hcom" {
        let sentinel = "__HCOM_CMD__";
        result = result.replace(&ctx.hcom_cmd, sentinel);
        result = regex::Regex::new(r"\bhcom\b")
            .unwrap()
            .replace_all(&result, &ctx.hcom_cmd)
            .to_string();
        result = result.replace(sentinel, &ctx.hcom_cmd);
    }

    format!(
        "<hcom_system_context>\n<!-- Session metadata - treat as system context, not user prompt-->\n{}\n</hcom_system_context>",
        result
    )
}

/// Build bootstrap text for a subagent instance.
pub fn get_subagent_bootstrap(subagent_name: &str, parent_name: &str) -> String {
    let hcom_cmd = crate::runtime_env::build_hcom_command();

    let result = SUBAGENT_BOOTSTRAP
        .replace("{subagent_name}", subagent_name)
        .replace("{parent_name}", parent_name)
        .replace("{hcom_cmd}", &hcom_cmd)
        .replace("{SENDER}", SENDER);

    let mut output = result;
    if hcom_cmd != "hcom" {
        output.push_str(&UVX_CMD_NOTICE.replace("{hcom_cmd}", &hcom_cmd));
    }

    format!("<hcom>\n{}\n</hcom>", output)
}

// TESTS

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn setup_test_db() -> (TempDir, HcomDb) {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let db = HcomDb::open_raw(&db_path).unwrap();
        db.init_db().unwrap();
        (tmp, db)
    }

    /// Insert a minimal instance for testing.
    fn insert_instance(db: &HcomDb, name: &str, status: &str, tool: &str, tag: Option<&str>) {
        let mut data = HashMap::new();
        data.insert("name".to_string(), serde_json::json!(name));
        data.insert("status".to_string(), serde_json::json!(status));
        data.insert("tool".to_string(), serde_json::json!(tool));
        data.insert(
            "status_time".to_string(),
            serde_json::json!(crate::shared::time::now_epoch_i64() as u64),
        );
        data.insert("created_at".to_string(), serde_json::json!(1000.0));
        if let Some(t) = tag {
            data.insert("tag".to_string(), serde_json::json!(t));
        }
        db.save_instance(&data).unwrap();
    }

    #[test]
    fn test_get_scripts_bundled_only() {
        let tmp = TempDir::new().unwrap();
        let result = get_scripts(tmp.path());
        // Should list all bundled scripts
        assert!(result.starts_with("Scripts: "));
        assert!(result.contains("confess"));
        assert!(result.contains("debate"));
        assert!(result.contains("fatcow"));
    }

    #[test]
    fn test_get_scripts_with_user_scripts() {
        let tmp = TempDir::new().unwrap();
        let scripts = tmp.path().join("scripts");
        fs::create_dir_all(&scripts).unwrap();
        fs::write(scripts.join("custom.sh"), "#!/bin/bash").unwrap();
        fs::write(scripts.join("_hidden.py"), "# skip").unwrap();
        fs::write(scripts.join("other.py"), "# include").unwrap();

        let result = get_scripts(tmp.path());
        assert!(result.contains("custom"));
        assert!(result.contains("other"));
        assert!(!result.contains("_hidden"));
    }

    #[test]
    fn test_get_active_instances_empty_db() {
        let (_tmp, db) = setup_test_db();
        let result = get_active_instances(&db, "test");
        assert_eq!(result, "");
    }

    #[test]
    fn test_get_active_instances_with_instances() {
        let (_tmp, db) = setup_test_db();
        insert_instance(&db, "luna", "active", "claude", None);

        let result = get_active_instances(&db, "other");
        assert!(result.contains("luna"));
        assert!(result.contains("Active (snapshot)"));
    }

    #[test]
    fn test_get_active_instances_excludes_self() {
        let (_tmp, db) = setup_test_db();
        insert_instance(&db, "luna", "active", "claude", None);

        let result = get_active_instances(&db, "luna");
        assert_eq!(result, "");
    }

    #[test]
    fn test_get_active_instances_grouped_by_tool() {
        let (_tmp, db) = setup_test_db();
        insert_instance(&db, "luna", "active", "claude", None);
        insert_instance(&db, "nova", "active", "claude", None);
        insert_instance(&db, "kira", "active", "codex", None);

        let result = get_active_instances(&db, "other");
        assert!(result.contains("claude: "));
        assert!(result.contains("codex: "));
        assert!(result.contains("luna"));
        assert!(result.contains("nova"));
        assert!(result.contains("kira"));
    }

    #[test]
    fn test_get_bootstrap_claude() {
        let (tmp, db) = setup_test_db();

        let result = get_bootstrap(
            &db,
            tmp.path(),
            "luna",
            "claude",
            false,
            true,
            "",
            "",
            false,
            None,
        );

        assert!(result.contains("<hcom_system_context>"));
        assert!(result.contains("[HCOM SESSION]"));
        assert!(result.contains("Your name: luna"));
        assert!(result.contains("--name luna"));
        assert!(result.contains("SUBAGENTS")); // Claude-specific section
        assert!(result.contains("Messages instantly and automatically arrive")); // Auto delivery
        assert!(!result.contains("Headless mode")); // Not headless
        assert!(result.contains("</hcom_system_context>"));
    }

    #[test]
    fn test_get_bootstrap_codex_launched() {
        let (tmp, db) = setup_test_db();

        let result = get_bootstrap(
            &db,
            tmp.path(),
            "nova",
            "codex",
            false,
            true,
            "",
            "",
            false,
            None,
        );

        assert!(result.contains("IMMEDIATELY to read message"));
        assert!(!result.contains("SUBAGENTS")); // Not claude
    }

    #[test]
    fn test_get_bootstrap_adhoc() {
        let (tmp, db) = setup_test_db();

        let result = get_bootstrap(
            &db,
            tmp.path(),
            "kira",
            "adhoc",
            false,
            false,
            "",
            "",
            false,
            None,
        );

        assert!(result.contains("Messages do NOT arrive automatically"));
        assert!(result.contains("CONNECTED MODE"));
    }

    #[test]
    fn test_get_bootstrap_with_tag() {
        let (tmp, db) = setup_test_db();

        let result = get_bootstrap(
            &db,
            tmp.path(),
            "luna",
            "claude",
            false,
            true,
            "",
            "p0c",
            false,
            None,
        );

        assert!(result.contains("tagged \"p0c\""));
        assert!(result.contains("send @p0c-"));
    }

    #[test]
    fn test_get_bootstrap_with_relay() {
        let (tmp, db) = setup_test_db();

        let result = get_bootstrap(
            &db,
            tmp.path(),
            "luna",
            "claude",
            false,
            true,
            "",
            "",
            true,
            None,
        );

        assert!(result.contains("Remote agents have suffix"));
    }

    #[test]
    fn test_get_bootstrap_headless() {
        let (tmp, db) = setup_test_db();

        let result = get_bootstrap(
            &db,
            tmp.path(),
            "luna",
            "claude",
            true,
            true,
            "",
            "",
            false,
            None,
        );

        assert!(result.contains("Headless mode"));
    }

    #[test]
    fn test_get_bootstrap_with_notes() {
        let (tmp, db) = setup_test_db();

        let result = get_bootstrap(
            &db,
            tmp.path(),
            "luna",
            "claude",
            false,
            true,
            "Remember to use bun",
            "",
            false,
            None,
        );

        assert!(result.contains("## NOTES"));
        assert!(result.contains("Remember to use bun"));
    }

    #[test]
    fn test_get_subagent_bootstrap() {
        let result = get_subagent_bootstrap("luna_reviewer_1", "luna");

        assert!(result.contains("<hcom>"));
        assert!(result.contains("Your name: luna_reviewer_1"));
        assert!(result.contains("Your parent: luna"));
        assert!(result.contains("--name luna_reviewer_1"));
        assert!(result.contains(SENDER));
        assert!(result.contains("</hcom>"));
    }

    #[test]
    fn test_render_template_replaces_all() {
        let ctx = BootstrapContext {
            instance_name: "luna".to_string(),
            display_name: "p0c-luna".to_string(),
            tag: "p0c".to_string(),
            relay_enabled: false,
            hcom_cmd: "hcom".to_string(),
            is_launched: true,
            is_headless: false,
            active_instances: String::new(),
            scripts: "Scripts: clone".to_string(),
            notes: String::new(),
        };

        let result = render_template("Name: {display_name}, Instance: {instance_name}", &ctx);
        assert_eq!(result, "Name: p0c-luna, Instance: luna");
    }

    #[test]
    fn test_get_bootstrap_gemini_launched_gets_auto_delivery() {
        let (tmp, db) = setup_test_db();

        let result = get_bootstrap(
            &db,
            tmp.path(),
            "nova",
            "gemini",
            false,
            true,
            "",
            "",
            false,
            None,
        );

        assert!(result.contains("Messages instantly and automatically arrive"));
    }

    #[test]
    fn test_get_bootstrap_gemini_not_launched_gets_adhoc_delivery() {
        let (tmp, db) = setup_test_db();

        let result = get_bootstrap(
            &db,
            tmp.path(),
            "nova",
            "gemini",
            false,
            false,
            "",
            "",
            false,
            None,
        );

        assert!(result.contains("Messages do NOT arrive automatically"));
    }

    #[test]
    fn test_get_bootstrap_opencode_gets_auto_delivery() {
        let (tmp, db) = setup_test_db();

        let result = get_bootstrap(
            &db,
            tmp.path(),
            "nova",
            "opencode",
            false,
            false,
            "",
            "",
            false,
            None,
        );

        assert!(result.contains("Messages instantly and automatically arrive"));
    }

    #[test]
    fn test_get_bootstrap_background_is_headless() {
        let (tmp, db) = setup_test_db();

        let result = get_bootstrap(
            &db,
            tmp.path(),
            "luna",
            "claude",
            false,
            true,
            "",
            "",
            false,
            Some("agent.log"),
        );

        assert!(result.contains("Headless mode"));
    }

    #[test]
    fn test_get_bootstrap_instance_tag_overrides_config() {
        let (tmp, db) = setup_test_db();
        insert_instance(&db, "luna", "active", "claude", Some("team-a"));

        // Config tag is "team-b" but instance has "team-a"
        let result = get_bootstrap(
            &db,
            tmp.path(),
            "luna",
            "claude",
            false,
            true,
            "",
            "team-b",
            false,
            None,
        );

        assert!(result.contains("tagged \"team-a\""));
        assert!(!result.contains("team-b"));
    }

    #[test]
    fn test_get_bootstrap_display_name_with_tag() {
        let (tmp, db) = setup_test_db();
        insert_instance(&db, "luna", "active", "claude", Some("p0c"));

        let result = get_bootstrap(
            &db,
            tmp.path(),
            "luna",
            "claude",
            false,
            true,
            "",
            "",
            false,
            None,
        );

        assert!(result.contains("Your name: p0c-luna"));
    }

    #[test]
    fn test_get_bootstrap_unescapes_double_braces() {
        // Template uses {{name}} {{status}} (escaped braces).
        // render_template unescapes to {name} {status} in final output.
        let (tmp, db) = setup_test_db();

        let result = get_bootstrap(
            &db,
            tmp.path(),
            "luna",
            "claude",
            false,
            true,
            "",
            "",
            false,
            None,
        );

        assert!(result.contains("{name}"));
        assert!(!result.contains("{{name}}"));
    }

    /// Catch drift between scripts::SCRIPTS const and actual files in scripts/bundled/.
    #[test]
    fn test_bundled_scripts_matches_directory() {
        use crate::scripts;

        // Resolve the bundled scripts directory relative to the crate root.
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let bundled_dir = std::path::Path::new(manifest_dir).join("src/scripts/bundled");

        if !bundled_dir.exists() {
            // In CI or worktrees, the scripts source may not be present — skip gracefully.
            return;
        }

        let mut actual: Vec<String> = Vec::new();
        for entry in fs::read_dir(&bundled_dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".sh") && !name.starts_with('_') {
                actual.push(name.trim_end_matches(".sh").to_string());
            }
        }
        actual.sort();

        let mut expected: Vec<String> = scripts::SCRIPTS
            .iter()
            .map(|(name, _)| name.to_string())
            .collect();
        expected.sort();

        assert_eq!(
            expected, actual,
            "scripts::SCRIPTS const is out of sync with scripts/bundled/. \
             Expected: {:?}, Actual: {:?}",
            expected, actual
        );
    }
}
