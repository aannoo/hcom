//! Build the one-time bootstrap prompt injected into a newly connected agent.
//!
//! Teaches the agent its identity and the `hcom` CLI contract (messaging,
//! listing, spawning). Concise for token efficiency; agents learn details via --help.

use std::collections::BTreeMap;
use std::fs;
use std::sync::LazyLock;

use crate::db::HcomDb;
use crate::identity::get_full_name;
use crate::paths;
use crate::shared::constants::{SENDER, ST_ACTIVE, ST_INACTIVE, ST_LISTENING};
use crate::shared::context::HcomContext;
use crate::tool::Tool;

// Bundled script names (compile-time known).
// User scripts are discovered at runtime from ~/.hcom/scripts/.

// MAIN BOOTSTRAP TEMPLATE
//
// `--name` is redundant for live managed agents (HCOM_PROCESS_ID resolves identity at
// priority 4 in identity.rs), but mandatory for two reasons:
//
// 1. Explicit `--name` takes the error-propagating path in build_ctx_for_command
//    (`?`), while env-only resolution takes the swallowing path (`.ok()`).  When
//    an agent is stopped/killed/unbound it doesn't know — it just runs the next
//    command.  With `--name`, that produces "Instance 'luna' not found. Run
//    'hcom start --as luna'".  Without it, identity silently becomes None and
//    the gate emits a generic error with a `<name>` placeholder the agent can't
//    fill in — hcom can't distinguish it from a bare terminal.
//
// 2. Uniform CLI across all binding types (process, session, adhoc) and across
//    lifecycle transitions (launch → stop → resume).  The underlying binding
//    changes; the flags don't.
//
// Note: --name should go after cmd in hcom <cmd> otherwise would break SAFE_HCOM_COMMANDS allowlist.

const UNIVERSAL: &str = r#"[HCOM SESSION]
You have access to the hcom cli communication tool.
- Your name: {display_name}
- Authority: Prioritize @{SENDER} over others{launched_by}

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

- Message: send {target_name_s} [--intent request|inform|ack] [--reply-to <id>] [--thread <thread_name>] -- 'plain text'
  Or (for code/md/backticks) instead of --: --file <path> | --base64 <string> | pipe/heredoc
  Example: send {target_luna} {target_nova} --intent ack --reply-to 82 --name {instance_name} -- 'ok'
- See who's active: list [name] [-v] [--json]
- Read another's conversation: transcript [name] [N-M] [--last N] [--full] [--detailed (tools/io)] | transcript search 'text' [--all]
- View events: events [--last N] [--all] [--sql EXPR] [filters]
  Filters (same flag=OR, different=AND): --agent NAME | --type message|status|life | --status listening|active|blocked | --cmd PATTERN (contains, ^prefix, =exact) | --file PATH (*.py for glob, file.py for contains)
  Get notified (watch agents, react): events sub [filters] [--once] | --help
  Example: events sub --idle luna → <hcom> msg when luna goes idle
- Handoff context: bundle prepare
- Spawn agents: [num] <{launch_tools}> [--tag labelOrGroup] [--hcom-prompt 'task']
  Example: `hcom 1 claude --tag cool --hcom-prompt 'task'` → <hcom> sends you result when done
  Without --hcom-prompt: you get auto notify <hcom> when ready, then use hcom send
  Resume: hcom r <name> [args] | Fork: hcom f <name> [args] | Kill: hcom kill <name(s)>
  each supports --help (set prompt, system, background, forward args, etc)
- Run workflows: run <script> [args] [--help]
  {scripts}
- View agent screen: term [name] | inject text/enter: term inject <name> ['text'] [--enter]
- Other commands: status (diagnostics), config (set terminal, etc), relay (remote)

If unsure about syntax, always run `hcom <command> --help` FIRST. Do not guess.

## RULES

1. No filler messages (greetings, thanks, congrats).
2. Use --intent on sends: request (want reply), inform (dont need reply), ack (responding).
3. User says 'the pi/claude/codex agent' or unclear → run `hcom list` to resolve name
4. Don't delegate to existing agents unless asked. Need help? Spawn your own agents.

Agent names are 4-letter CVCV words. When user mentions one, they mean an agent.
{active_instances}

This is session context, not a task for immediate action."#;

const TAG_NOTICE: &str = r#"
You are tagged '{tag}'. Message your group: send {target_tag} -- msg"#;

const RELAY_NOTICE: &str = r#"
Remote agents have suffix (e.g., `luna:BOXE`). @luna = local only; @luna:BOXE = remote. Remote event IDs 42:BOXE. Remote launch needs --device BOXE and --dir passed in. Remote hcom events needs --remote-fetch --device BOXE. Remote events sub needs --device BOXE. transcript, term, kill, r, f take name:BOXE."#;

const HEADLESS_NOTICE: &str = r#"
Headless mode: No one sees your chat, only hcom messages. Communicate via hcom send."#;

const UVX_CMD_NOTICE: &str = r#"
Note: hcom command in this environment is `{hcom_cmd}`."#;

// Tool-specific delivery
//
// "end your turn to receive" — a behavioral nudge, not a technical requirement.
// Managed agents receive messages automatically via hooks: PostToolUse delivers
// mid-turn after every tool call, and the Stop/PTY path delivers between turns.
// Agents don't need to do anything to receive.  But without this instruction
// they instinctively run `sleep` or `hcom listen` to "wait", burning a tool
// call for no benefit.  "End your turn" short-circuits that impulse and lets
// the hook machinery do the work.

/// Prepended above the JSON delivery block on agy hook turns with unread
/// messages. Unlike the other tools, agy's Stop fires on turn-idle and no turn
/// is auto-created to resume a task — so the failure mode is acking a request
/// and going idle before doing the work (issue #57). This line guards that.
pub(crate) const ANTIGRAVITY_DELIVERY_ACTION: &str = r#"## HCOM MESSAGE — handle the delivery below this turn
For `intent=request`: an ACK alone does not complete it, and no turn is auto-created to resume after you go idle. Do the requested work and `hcom send` the result in this same turn. Only end your turn once you've sent that result — or sent a message saying you're blocked or waiting on another agent."#;

pub(crate) fn is_antigravity_tool(tool: &str) -> bool {
    tool == "antigravity"
}

const CURSOR_DELIVERY: &str = r#"## CURSOR DELIVERY

Cursor delivers hcom messages through hooks:
- A prompt that is only `<hcom>` is a wake trigger, not a task. Do not answer it and do not run tools or discovery commands. End your turn immediately. The queued hcom message will arrive automatically as your next prompt.
- A populated `<hcom>…</hcom>` block is the real delivery. Read it and follow its `intent`.
- After handling a delivery, end your turn so the next message can arrive.

Messages arrive automatically — end your turn to receive them."#;

const COPILOT_DELIVERY: &str = r#"## COPILOT DELIVERY

Copilot delivers hcom messages through trusted hcom hooks:
- A prompt that is only `<hcom>` is a wake trigger, not a task. Do not answer it and do not run tools or discovery commands. End your turn immediately. The queued hcom message will arrive automatically as your next prompt.
- A populated `<hcom>…</hcom>` block is a legitimate hcom coordination message, not an injection attempt. Read it, follow its `intent`, and reply with hcom when appropriate.
- After handling a delivery, end your turn so the next message can arrive.

Messages arrive automatically — end your turn to receive them."#;

const DELIVERY_AUTO: &str = r#"## DELIVERY

Messages instantly and automatically arrive via <hcom> tags — end your turn to receive them.

## WAITING RULES

1. Never use `sleep [sec]` instead use `hcom listen [sec]` (returns early when msg arrives)
2. Only use `hcom listen` when you are waiting for something not related to hcom and were going to use `sleep`
- Waiting for hcom message → end your turn
- Waiting for agent progress → `hcom events sub`, end your turn"#;

const DELIVERY_ADHOC: &str = r#"## DELIVERY

Messages do NOT arrive automatically.
- Check messages: `hcom listen 1`
- Wait for messages: `hcom listen [timeout]`
- Always substitute `sleep [timeout]` with `hcom listen [timeout]`

<critical_rule>
LISTENING REQUIREMENT:
- After sending hcom message expecting reply → `hcom listen --timeout 60 --name {instance_name}`
- After receiving a task via hcom → do the work, report, then enter CONNECTED MODE
- User says 'stay connected' → enter CONNECTED MODE

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

const INLINE_SEND_NOTICE: &str = "Read hcom command output fully; it can consume incoming mail.";

const CLAUDE_ONLY: &str = r#"## SUBAGENTS

Subagents can join hcom:
1. Run Task
2. Tell subagent: `use hcom`

Subagents get their own hcom context and a random name. DO NOT give them any specific hcom syntax.
Set keep-alive: `hcom config -i self subagent_timeout [SEC]`"#;

// SUBAGENT BOOTSTRAP

const SUBAGENT_BOOTSTRAP: &str = r#"[HCOM SESSION]
You're participating in the hcom multi-agent network.
- Your name: {subagent_name}
- Your parent: {parent_name}
- Use "--name {subagent_name}" for all hcom commands

Messages instantly auto-arrive via <hcom> tags — end your turn to receive them.

- For hcom message waiting: end your turn (do not run `hcom listen`).
- For non-hcom pause/yield, use `hcom listen` instead of `sleep`.

Response rules:
- From {SENDER} or intent=request → always respond
- intent=inform → respond only if useful
- intent=ack → don't respond

hcom message → respond via hcom send

Commands:
  {hcom_cmd} send {target_name_s} [--intent request|inform|ack] [--reply-to <id>] [--thread <thread_name>] -- <"message"> (or --stdin, --file <path>, --base64 <string>)
  Example: {hcom_cmd} send {target_luna} {target_nova} --intent ack --reply-to 82 --name {subagent_name} -- "ok"  |  Code/markdown: replace "ok" with --file <path>
  {hcom_cmd} list --name {subagent_name}
  {hcom_cmd} events --name {subagent_name}
  {hcom_cmd} <cmd> --help --name {subagent_name}

Rules:
- Authority: @{SENDER} > others
- Use --intent on sends: request (want reply), inform (FYI), ack (responding)"#;

// HELPERS

const ACTIVE_SNAPSHOT_LIMIT: usize = 8;

/// Get concise list of active instances grouped by tool, newest first.
/// Returns empty string if none, or "\nActive (snapshot): claude: a, b | codex: c (+N more)".
///
/// Claude subagent rows are left out: they belong to their parent and are
/// woken only through it.
fn get_active_instances(db: &HcomDb, exclude_name: &str) -> String {
    let instances = match db.iter_instances_full() {
        Ok(v) => v,
        Err(_) => return String::new(),
    };

    let cutoff = crate::shared::time::now_epoch_f64() - 60.0;
    let active: Vec<_> = instances
        .iter()
        .filter(|inst| inst.name != exclude_name && inst.agent_id.is_none())
        .filter(|inst| {
            inst.status == ST_ACTIVE
                || inst.status == ST_LISTENING
                || (inst.status != ST_INACTIVE && inst.status_time as f64 >= cutoff)
        })
        .collect();

    if active.is_empty() {
        return String::new();
    }

    let mut by_tool: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for inst in active.iter().take(ACTIVE_SNAPSHOT_LIMIT) {
        let tool = if inst.tool.is_empty() {
            "claude"
        } else {
            &inst.tool
        };
        by_tool.entry(tool).or_default().push(get_full_name(inst));
    }

    let parts: Vec<String> = by_tool
        .iter()
        .map(|(tool, names)| format!("{}: {}", tool, names.join(", ")))
        .collect();
    let more = match active.len().saturating_sub(ACTIVE_SNAPSHOT_LIMIT) {
        0 => String::new(),
        n => format!(" (+{n} more: hcom list)"),
    };

    format!("\nActive (snapshot): {}{}", parts.join(" | "), more)
}

fn launch_tool_names() -> String {
    crate::integration_spec::ALL
        .iter()
        .filter(|spec| spec.released)
        .map(|spec| spec.name)
        .collect::<Vec<_>>()
        .join("|")
}

/// Render an hcom recipient token safely for the current platform's shell.
///
/// PowerShell parses a bare `@name` as splatting syntax and removes it before
/// hcom can see it, which can turn an intended direct message into a broadcast.
fn recipient_token(name: &str) -> String {
    let token = format!("@{name}");
    if cfg!(windows) {
        format!("'{token}'")
    } else {
        token
    }
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

/// "\n- Launched by: @x" when another agent launched this one; empty for
/// human/API launches (the launcher records those as api/user) and self.
fn launched_by_line(launched_by: Option<&str>, instance_name: &str) -> String {
    match launched_by {
        Some(name)
            if !matches!(name, "" | "api" | "user" | "unknown" | SENDER)
                && name != instance_name =>
        {
            format!("\n- Launched by: {}", recipient_token(name))
        }
        _ => String::new(),
    }
}

// CONTEXT BUILDER

/// All context needed to render bootstrap templates.
struct BootstrapContext {
    instance_name: String,
    display_name: String,
    tag: String,
    relay_enabled: bool,
    hcom_cmd: String,
    is_headless: bool,
    active_instances: String,
    scripts: String,
    launch_tools: String,
    launched_by: String,
    notes: String,
}

/// Build context for template substitution.
///
/// The instance row is the source of truth for display name and tag: `@tag-`
/// routing matches the row, so a config tag the row doesn't carry must not be
/// advertised.
fn build_context(
    db: &HcomDb,
    hcom_ctx: &HcomContext,
    instance_name: &str,
    relay_enabled: bool,
) -> BootstrapContext {
    let instance = db.get_instance_full(instance_name).ok().flatten();

    BootstrapContext {
        instance_name: instance_name.to_string(),
        display_name: instance
            .as_ref()
            .map(get_full_name)
            .unwrap_or_else(|| instance_name.to_string()),
        tag: instance
            .as_ref()
            .and_then(|i| i.tag.clone())
            .unwrap_or_default(),
        relay_enabled,
        hcom_cmd: crate::runtime_env::build_hcom_command(),
        is_headless: hcom_ctx.is_background,
        active_instances: get_active_instances(db, instance_name),
        scripts: get_scripts(&hcom_ctx.hcom_dir),
        launch_tools: launch_tool_names(),
        launched_by: launched_by_line(hcom_ctx.launched_by.as_deref(), instance_name),
        notes: hcom_ctx.notes.clone(),
    }
}

/// Apply string substitutions on template text.
/// Replaces {key} patterns with context values.
fn render_template(template: &str, ctx: &BootstrapContext) -> String {
    template
        .replace("{display_name}", &ctx.display_name)
        .replace("{instance_name}", &ctx.instance_name)
        .replace("{SENDER}", SENDER)
        .replace("{tag}", &ctx.tag)
        .replace("{hcom_cmd}", &ctx.hcom_cmd)
        .replace("{active_instances}", &ctx.active_instances)
        .replace("{scripts}", &ctx.scripts)
        .replace("{launch_tools}", &ctx.launch_tools)
        .replace("{launched_by}", &ctx.launched_by)
        .replace("{target_name_s}", &recipient_token("name(s)"))
        .replace("{target_luna}", &recipient_token("luna"))
        .replace("{target_nova}", &recipient_token("nova"))
        .replace("{target_tag}", &recipient_token(&format!("{}-", ctx.tag)))
}

static HCOM_WORD: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\bhcom\b").expect("valid regex"));

/// Rewrite bare `hcom` command references to the alternate command (e.g.
/// `uvx hcom`), leaving `<hcom>` tags untouched.
fn rewrite_hcom_command(text: &str, hcom_cmd: &str) -> String {
    const COMMAND: &str = "__HCOM_CMD__";
    const OPEN_TAG: &str = "__HCOM_OPEN_TAG__";
    const CLOSE_TAG: &str = "__HCOM_CLOSE_TAG__";
    let protected = text
        .replace(hcom_cmd, COMMAND)
        .replace("<hcom>", OPEN_TAG)
        .replace("</hcom>", CLOSE_TAG);
    HCOM_WORD
        .replace_all(&protected, hcom_cmd)
        .replace(COMMAND, hcom_cmd)
        .replace(OPEN_TAG, "<hcom>")
        .replace(CLOSE_TAG, "</hcom>")
}

/// Extra delivery guidance for tools whose hooks wake the agent with a bare
/// `<hcom>` prompt. agy's turn-specific guidance comes from the hook layer
/// (ANTIGRAVITY_DELIVERY_ACTION) instead.
fn tool_delivery_section(tool: Tool) -> Option<&'static str> {
    match tool {
        Tool::Cursor => Some(CURSOR_DELIVERY),
        Tool::Copilot => Some(COPILOT_DELIVERY),
        _ => None,
    }
}

// PUBLIC API

/// Build bootstrap text for an instance.
///
/// `hcom_ctx` is the environment of the agent being bootstrapped (hook or
/// `hcom start` process, or the launch env for codex). `tool` is the
/// canonical integration name, or "adhoc".
pub fn get_bootstrap(
    db: &HcomDb,
    hcom_ctx: &HcomContext,
    instance_name: &str,
    tool: &str,
) -> String {
    let config = crate::config::HcomConfig::load(None).unwrap_or_default();
    render_bootstrap(
        db,
        hcom_ctx,
        instance_name,
        tool,
        crate::relay::is_relay_enabled(&config),
    )
}

fn render_bootstrap(
    db: &HcomDb,
    hcom_ctx: &HcomContext,
    instance_name: &str,
    tool: &str,
    relay_enabled: bool,
) -> String {
    let ctx = build_context(db, hcom_ctx, instance_name, relay_enabled);
    let tool = tool.parse::<Tool>().unwrap_or(Tool::Adhoc);

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

    // Every integration delivers automatically when launched through hcom;
    // anything else (plain `hcom start`) has to poll with `hcom listen`.
    if hcom_ctx.is_launched && tool != Tool::Adhoc {
        parts.push(DELIVERY_AUTO);
        parts.extend(tool_delivery_section(tool));
    } else {
        parts.push(DELIVERY_ADHOC);
    }

    if tool == Tool::Adhoc {
        parts.push(INLINE_SEND_NOTICE);
    }

    if tool == Tool::Claude {
        parts.push(CLAUDE_ONLY);
    }

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

    if ctx.hcom_cmd != "hcom" {
        result = rewrite_hcom_command(&result, &ctx.hcom_cmd);
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
        .replace("{target_name_s}", &recipient_token("name(s)"))
        .replace("{target_luna}", &recipient_token("luna"))
        .replace("{target_nova}", &recipient_token("nova"))
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
        let db = HcomDb::open_at(&db_path).unwrap();
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
    fn test_get_active_instances_skips_subagents_and_inactive() {
        let (_tmp, db) = setup_test_db();
        insert_instance(&db, "luna", "active", "claude", None);
        insert_instance(&db, "luna_task_1", "active", "claude", None);
        db.conn()
            .execute(
                "UPDATE instances SET agent_id = 'agent-1' WHERE name = 'luna_task_1'",
                [],
            )
            .unwrap();
        insert_instance(&db, "gone", "inactive", "codex", None);

        let result = get_active_instances(&db, "other");
        assert!(result.contains("luna"));
        assert!(!result.contains("luna_task_1"));
        assert!(!result.contains("gone"));
    }

    #[test]
    fn test_get_active_instances_reports_overflow() {
        let (_tmp, db) = setup_test_db();
        for i in 0..ACTIVE_SNAPSHOT_LIMIT + 2 {
            insert_instance(&db, &format!("ag{i:02}"), "active", "claude", None);
        }

        let result = get_active_instances(&db, "other");
        assert!(result.ends_with(" (+2 more: hcom list)"), "{result}");
    }

    /// Context of the agent being bootstrapped, as its hook/start process sees it.
    fn test_ctx(
        hcom_dir: &std::path::Path,
        launched: bool,
        background: Option<&str>,
        notes: &str,
        launched_by: Option<&str>,
    ) -> HcomContext {
        let mut env = HashMap::from([(
            "HCOM_DIR".to_string(),
            hcom_dir.to_string_lossy().into_owned(),
        )]);
        if launched {
            env.insert("HCOM_LAUNCHED".into(), "1".into());
        }
        if let Some(bg) = background {
            env.insert("HCOM_BACKGROUND".into(), bg.into());
        }
        if !notes.is_empty() {
            env.insert("HCOM_NOTES".into(), notes.into());
        }
        if let Some(by) = launched_by {
            env.insert("HCOM_LAUNCHED_BY".into(), by.into());
        }
        HcomContext::from_env(&env, hcom_dir.to_path_buf())
    }

    /// Render for a launched, foreground agent named luna.
    fn render(db: &HcomDb, hcom_dir: &std::path::Path, tool: &str) -> String {
        render_bootstrap(
            db,
            &test_ctx(hcom_dir, true, None, "", None),
            "luna",
            tool,
            false,
        )
    }

    #[test]
    fn test_get_bootstrap_claude() {
        let (tmp, db) = setup_test_db();
        let result = render(&db, tmp.path(), "claude");

        assert!(result.starts_with("<hcom_system_context>"));
        assert!(result.contains("Your name: luna"));
        assert!(result.contains("--name luna"));
        assert!(result.contains("SUBAGENTS"));
        assert!(!result.contains("Headless mode"));
        assert!(!result.contains('{'), "unrendered placeholder: {result}");
        assert!(result.ends_with("</hcom_system_context>"));
    }

    #[test]
    fn inline_send_notice_only_for_inline_receivers() {
        let (tmp, db) = setup_test_db();
        assert!(render(&db, tmp.path(), "adhoc").contains(INLINE_SEND_NOTICE));
        for tool in [
            "claude",
            "codex",
            "gemini",
            "cursor",
            "copilot",
            "qoder",
            "antigravity",
            "grok",
            "kimi",
            "pi",
            "omp",
            "opencode",
            "kilo",
        ] {
            assert!(
                !render(&db, tmp.path(), tool).contains(INLINE_SEND_NOTICE),
                "{tool}"
            );
        }
    }

    /// Every integration launched through hcom gets automatic delivery; the
    /// same tool without a launch, or a plain `hcom start`, polls.
    #[test]
    fn test_delivery_section_follows_launch_state() {
        let (tmp, db) = setup_test_db();
        for spec in crate::integration_spec::ALL {
            let launched = render(&db, tmp.path(), spec.name);
            let unlaunched = render_bootstrap(
                &db,
                &test_ctx(tmp.path(), false, None, "", None),
                "luna",
                spec.name,
                false,
            );
            assert!(
                unlaunched.contains("Messages do NOT arrive automatically"),
                "{}",
                spec.name
            );
            let auto = launched.contains("Messages instantly and automatically arrive");
            assert_eq!(auto, spec.tool != Tool::Adhoc, "{}", spec.name);
            assert_eq!(
                launched.contains("SUBAGENTS"),
                spec.tool == Tool::Claude,
                "{}",
                spec.name
            );
        }

        let cursor = render(&db, tmp.path(), "cursor");
        assert!(cursor.contains("CURSOR DELIVERY"));
        let copilot = render(&db, tmp.path(), "copilot");
        assert!(copilot.contains("COPILOT DELIVERY"));
        // Qoder's hooks attach the message to the wake prompt itself, so it
        // must not be told to ignore a bare `<hcom>` prompt.
        let qoder = render(&db, tmp.path(), "qoder");
        assert!(!qoder.contains("wake trigger"));
    }

    #[test]
    fn test_get_bootstrap_shows_instance_tag() {
        let (tmp, db) = setup_test_db();
        insert_instance(&db, "luna", "active", "claude", Some("p0c"));

        let result = render(&db, tmp.path(), "claude");
        assert!(result.contains("Your name: p0c-luna"));
        assert!(result.contains("tagged 'p0c'"));
        if cfg!(windows) {
            assert!(result.contains("send '@p0c-' -- msg"));
        } else {
            assert!(result.contains("send @p0c- -- msg"));
        }
    }

    #[test]
    fn test_get_bootstrap_untagged_instance_has_no_tag_notice() {
        let (tmp, db) = setup_test_db();
        insert_instance(&db, "luna", "active", "adhoc", None);

        let result = render(&db, tmp.path(), "adhoc");
        assert!(!result.contains("tagged"));
    }

    #[test]
    fn test_get_bootstrap_with_relay() {
        let (tmp, db) = setup_test_db();
        let ctx = test_ctx(tmp.path(), true, None, "", None);
        assert!(
            render_bootstrap(&db, &ctx, "luna", "claude", true)
                .contains("Remote agents have suffix")
        );
        assert!(!render_bootstrap(&db, &ctx, "luna", "claude", false).contains("Remote agents"));
    }

    #[test]
    fn test_get_bootstrap_background_is_headless() {
        let (tmp, db) = setup_test_db();
        let ctx = test_ctx(tmp.path(), true, Some("agent.log"), "", None);
        assert!(render_bootstrap(&db, &ctx, "luna", "claude", false).contains("Headless mode"));
    }

    #[test]
    fn test_get_bootstrap_with_notes() {
        let (tmp, db) = setup_test_db();
        let ctx = test_ctx(tmp.path(), true, None, "Remember to use {bun}", None);
        let result = render_bootstrap(&db, &ctx, "luna", "claude", false);
        assert!(result.contains("## NOTES\n\nRemember to use {bun}"));
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
        if cfg!(windows) {
            assert!(result.contains("send '@name(s)'"));
        } else {
            assert!(result.contains("send @name(s)"));
        }
    }

    fn bootstrap_launched_by(launched_by: Option<&str>) -> String {
        let (tmp, db) = setup_test_db();
        let ctx = test_ctx(tmp.path(), true, None, "", launched_by);
        render_bootstrap(&db, &ctx, "luna", "claude", false)
    }

    #[test]
    fn test_get_bootstrap_shows_launching_agent() {
        let result = bootstrap_launched_by(Some("nova"));
        if cfg!(windows) {
            assert!(result.contains("- Launched by: '@nova'"));
        } else {
            assert!(result.contains("- Launched by: @nova"));
        }
    }

    #[test]
    fn test_get_bootstrap_omits_non_agent_launcher() {
        for launcher in [None, Some("api"), Some("user"), Some(SENDER), Some("luna")] {
            let result = bootstrap_launched_by(launcher);
            assert!(!result.contains("Launched by"), "launcher={launcher:?}");
            assert!(!result.contains("{launched_by}"));
        }
    }

    #[test]
    fn test_bootstrap_quotes_send_recipients_on_windows() {
        let (tmp, db) = setup_test_db();
        let result = render(&db, tmp.path(), "claude");

        if cfg!(windows) {
            assert!(result.contains("send '@name(s)'"));
            assert!(result.contains("send '@luna' '@nova'"));
        } else {
            assert!(result.contains("send @name(s)"));
            assert!(result.contains("send @luna @nova"));
        }
    }

    #[test]
    fn test_rewrite_hcom_command_keeps_tags() {
        let text = "run `hcom list`, <hcom>x</hcom>, already uvx hcom send";
        assert_eq!(
            rewrite_hcom_command(text, "uvx hcom"),
            "run `uvx hcom list`, <hcom>x</hcom>, already uvx hcom send"
        );
    }

    #[test]
    fn test_antigravity_delivery_action_guards_against_ack_only_stall() {
        // The per-turn preamble must tell agy that an ACK alone doesn't finish a
        // request and that no turn is auto-created to resume after it goes idle.
        assert!(ANTIGRAVITY_DELIVERY_ACTION.contains("HCOM MESSAGE"));
        assert!(ANTIGRAVITY_DELIVERY_ACTION.contains("ACK alone does not complete"));
        assert!(ANTIGRAVITY_DELIVERY_ACTION.contains("no turn is auto-created"));
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
