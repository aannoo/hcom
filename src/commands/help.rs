//! Native help text for all hcom commands.
//!
//! Each command has a list of (usage, description) entries formatted by `get_command_help()`.

use std::env;

/// Help entry: (usage, description). See `format_entry()` for rendering rules.
type HelpEntry = (&'static str, &'static str);

// ── Shared filter help (events, listen, events sub) ─────────────────────

const FILTER_HELP: &[HelpEntry] = &[
    ("  --agent NAME", "Agent name"),
    ("  --type TYPE", "message | status | life"),
    ("  --status VAL", "listening | active | blocked"),
    (
        "  --context PATTERN",
        "tool:Bash | deliver:X (supports * wildcard)",
    ),
    (
        "  --action VAL",
        "created | started | ready | stopped | batch_launched",
    ),
    (
        "  --cmd PATTERN",
        "Shell command (contains, ^prefix, =exact)",
    ),
    (
        "  --file PATH",
        "File write (*.py for glob, file.py for contains)",
    ),
    ("  --collision", "Two agents edit same file within 30s"),
    ("  --from NAME", "Sender"),
    ("  --mention NAME", "@mention target"),
    ("  --intent VAL", "request | inform | ack"),
    ("  --thread NAME", "Thread name"),
    ("  --after TIME", "After timestamp (ISO-8601)"),
    ("  --before TIME", "Before timestamp (ISO-8601)"),
];

// ── Per-command help registries ─────────────────────────────────────────

const EVENTS_HELP: &[HelpEntry] = &[
    (
        "",
        "Query the event stream (messages, status changes, file edits, lifecycle)",
    ),
    ("", ""),
    ("Query:", ""),
    ("  events", "Last 20 events as JSON"),
    ("  --last N", "Limit count (default: 20)"),
    ("  --all", "Include archived sessions"),
    ("  --wait [SEC]", "Block until match (default: 60s)"),
    ("  --sql EXPR", "Raw SQL WHERE (ANDed with flags)"),
];

// events help continued after FILTER_HELP splice
const EVENTS_HELP_2: &[HelpEntry] = &[
    ("", ""),
    ("Shortcuts:", ""),
    ("  --idle NAME", "--agent NAME --status listening"),
    ("  --blocked NAME", "--agent NAME --status blocked"),
    ("", ""),
    (
        "Subscribe (next matching event delivered as messages from [hcom-events]):",
        "",
    ),
    ("  events sub list", "List active subscriptions"),
    (
        "  events sub [filters] [--once]",
        "Subscribe using filter flags (listed above)",
    ),
    (
        "  events sub \"SQL WHERE\" [--once]",
        "Subscribe using raw SQL",
    ),
    ("    --once", "Auto-remove after first match"),
    ("    --for <name>", "Subscribe on behalf of another agent"),
    ("  events unsub <id>", "Remove subscription"),
    ("", ""),
    ("Examples:", ""),
    ("  events --cmd git --agent peso", ""),
    ("  events sub --idle peso", "Notified when peso goes idle"),
    (
        "  events sub --file '*.py' --once",
        "One-shot: next .py file write",
    ),
    ("", ""),
    ("SQL reference (events_v view):", ""),
    ("  Base", "id, timestamp, type, instance"),
    (
        "  msg_*",
        "from, text, scope, sender_kind, delivered_to[], mentions[], intent, thread, reply_to",
    ),
    ("  status_*", "val, context, detail"),
    ("  life_*", "action, by, batch_id, reason"),
    ("", ""),
    ("  type", "message, status, life"),
    ("  msg_scope", "broadcast, mentions"),
    ("  msg_sender_kind", "instance, external, system"),
    (
        "  status_context",
        "tool:X, deliver:X, approval, prompt, exit:X",
    ),
    (
        "  life_action",
        "created, started, ready, stopped, batch_launched",
    ),
    ("", ""),
    (
        "",
        "delivered_to/mentions are JSON arrays \u{2014} use LIKE '%name%' not = 'name'",
    ),
    ("", "Use <> instead of != for SQL negation"),
];

const LIST_HELP: &[HelpEntry] = &[
    ("list", "All alive agents, read receipts"),
    ("  -v", "Verbose (directory, session, etc)"),
    ("  --json", "JSON array of all agents"),
    ("  --names", "Just names, one per line"),
    (
        "  --format TPL",
        "Template per agent: --format '{name} {status}'",
    ),
    (
        "",
        "  Fields: name, base_name, status, status_context, status_detail,",
    ),
    (
        "",
        "  status_age_seconds, description, unread_count, tool, tag, directory,",
    ),
    (
        "",
        "  session_id, parent_name, agent_id, headless, created_at,",
    ),
    (
        "",
        "  hooks_bound, process_bound, transcript_path, background_log_file,",
    ),
    ("", "  launch_context"),
    ("", ""),
    ("list [self|<name>]", "Single agent details"),
    (
        "  [field]",
        "Print specific field (status, directory, session_id, ...)",
    ),
    ("  --json", "Output as JSON"),
    ("  --sh", "Shell exports: eval \"$(hcom list self --sh)\""),
    ("", ""),
    ("list --stopped [name]", "Stopped agents (from events)"),
    ("  --all", "All stopped (default: last 20)"),
    ("", ""),
    ("Status icons:", ""),
    (
        "",
        "\u{25b6}  active      processing, reads messages very soon",
    ),
    ("", "\u{25c9}  listening   idle, reads messages in <1s"),
    ("", "\u{25a0}  blocked     needs human approval"),
    ("", "\u{25cb}  inactive    dead or stale"),
    ("", "\u{25e6}  unknown     neutral"),
    ("", ""),
    ("Tool labels:", ""),
    (
        "",
        "[CLAUDE] [GEMINI] [CODEX] [OPENCODE]  hcom-launched (PTY + hooks)",
    ),
    (
        "",
        "[claude] [gemini] [codex] [opencode]  vanilla (hooks only)",
    ),
    ("", "[AD-HOC]                              manual polling"),
];

const SEND_HELP: &[HelpEntry] = &[
    ("  send @name -- message text", "Direct message"),
    ("  send @name1 @name2 -- message", "Multiple targets"),
    ("  send -- message text", "Broadcast to all"),
    ("  send @name", "Message from stdin (pipe or heredoc)"),
    ("  send @name --file <path>", "Message from file"),
    (
        "  send @name --base64 <encoded>",
        "Message from base64 string",
    ),
    ("", ""),
    ("", "Everything after -- is the message (no quotes needed)."),
    ("", "All flags must come before --."),
    ("", ""),
    ("Target matching:", ""),
    ("  @luna", "base name (matches luna, api-luna)"),
    ("  @api-luna", "exact full name"),
    ("  @api-", "prefix: all with tag 'api'"),
    ("  @luna:BOXE", "remote agent on another device"),
    (
        "",
        "Underscore blocks prefix: @luna does NOT match luna_reviewer_1",
    ),
    ("", ""),
    ("Envelope:", ""),
    ("  --intent <type>", "request | inform | ack"),
    ("", "  request: expect a response"),
    ("", "  inform: FYI, no response needed"),
    ("", "  ack: replying to a request (requires --reply-to)"),
    ("  --reply-to <id>", "Link to event ID (42 or 42:BOXE)"),
    ("  --thread <name>", "Group related messages"),
    ("", ""),
    ("Sender:", ""),
    ("  --from <name>", "External sender identity (alias: -b)"),
    ("  --name <name>", "Your identity (agent name or UUID)"),
    ("", ""),
    ("Inline bundle (attach structured context):", ""),
    ("  --title <text>", "Create and attach bundle inline"),
    (
        "  --description <text>",
        "Bundle description (required with --title)",
    ),
    ("  --events <ids>", "Event IDs/ranges: 1,2,5-10"),
    ("  --files <paths>", "Comma-separated file paths"),
    (
        "  --transcript <ranges>",
        "Format: 3-14:normal,6:full,22-30:detailed",
    ),
    ("  --extends <id>", "Parent bundle (optional)"),
    ("", "See 'hcom bundle --help' for bundle details"),
    ("", ""),
    ("Examples:", ""),
    ("  hcom send @luna -- Hello there!", ""),
    (
        "  hcom send @luna @nova --intent request -- Can you help?",
        "",
    ),
    ("  hcom send -- Broadcast message to everyone", ""),
    ("  echo 'Complex message' | hcom send @luna", ""),
    ("  hcom send @luna <<'EOF'", ""),
    ("  Multi-line message with special chars", ""),
    ("  EOF", ""),
];

const BUNDLE_HELP: &[HelpEntry] = &[
    ("bundle", "List recent bundles (alias: bundle list)"),
    ("bundle list", "List recent bundles"),
    ("  --last N", "Limit count (default: 20)"),
    ("  --json", "Output JSON"),
    ("", ""),
    ("bundle cat <id>", "Expand full bundle content"),
    (
        "",
        "Shows: metadata, files (metadata only), transcript (respects detail level), events",
    ),
    ("", ""),
    ("bundle prepare", "Show recent context, suggest template"),
    (
        "  --for <agent>",
        "Prepare for specific agent (default: self)",
    ),
    (
        "  --last-transcript N",
        "Transcript entries to suggest (default: 20)",
    ),
    (
        "  --last-events N",
        "Events to scan per category (default: 30)",
    ),
    ("  --json", "Output JSON"),
    ("  --compact", "Hide how-to section"),
    (
        "",
        "Shows suggested transcript ranges, relevant events, files",
    ),
    ("", "Outputs ready-to-use bundle create command"),
    (
        "",
        "TIP: Skip 'bundle create' \u{2014} use bundle flags directly in 'hcom send'",
    ),
    ("", ""),
    ("bundle show <id>", "Show bundle by id/prefix"),
    ("  --json", "Output JSON"),
    ("", ""),
    (
        "bundle create \"title\"",
        "Create bundle (positional or --title)",
    ),
    (
        "  --title <text>",
        "Bundle title (alternative to positional)",
    ),
    ("  --description <text>", "Bundle description (required)"),
    (
        "  --events 1,2,5-10",
        "Event IDs/ranges, comma-separated (required)",
    ),
    (
        "  --files a.py,b.py",
        "Comma-separated file paths (required)",
    ),
    (
        "  --transcript RANGES",
        "Transcript with detail levels (required)",
    ),
    (
        "",
        "    Format: range:detail (3-14:normal,6:full,22-30:detailed)",
    ),
    (
        "",
        "    normal = truncated | full = complete | detailed = tools+edits",
    ),
    ("  --extends <id>", "Parent bundle for chaining"),
    ("  --bundle JSON", "Create from JSON payload"),
    ("  --bundle-file FILE", "Create from JSON file"),
    ("  --json", "Output JSON"),
    ("", ""),
    ("JSON format:", ""),
    ("", "{"),
    ("", "  \"title\": \"Bundle Title\","),
    (
        "",
        "  \"description\": \"What happened, decisions, state, next steps\",",
    ),
    ("", "  \"refs\": {"),
    ("", "    \"events\": [\"123\", \"124-130\"],"),
    (
        "",
        "    \"files\": [\"src/auth.py\", \"tests/test_auth.py\"],",
    ),
    (
        "",
        "    \"transcript\": [\"10-15:normal\", \"20:full\", \"30-35:detailed\"]",
    ),
    ("", "  },"),
    ("", "  \"extends\": \"bundle:abc123\""),
    ("", "}"),
    ("", ""),
    ("bundle chain <id>", "Show bundle lineage"),
    ("  --json", "Output JSON"),
];

const STOP_HELP: &[HelpEntry] = &[
    ("stop", "Disconnect self from hcom"),
    ("stop <name>", "Disconnect specific agent"),
    ("stop <n1> <n2> ...", "Disconnect multiple"),
    ("stop tag:<name>", "Disconnect all with tag"),
    ("stop all", "Disconnect all agents"),
    ("", ""),
];

const START_HELP: &[HelpEntry] = &[
    ("start", "Connect to hcom (from inside any AI session)"),
    (
        "start --as <name>",
        "Reclaim identity (after compaction/resume/clear)",
    ),
    (
        "start --orphan <name|pid>",
        "Recover orphaned PTY process from pidtrack",
    ),
    ("", ""),
    ("", ""),
    (
        "",
        "Inside a sandbox? Prefix all hcom commands with: HCOM_DIR=$PWD/.hcom",
    ),
];

const KILL_HELP: &[HelpEntry] = &[
    ("kill <name>", "Kill process (+ close terminal pane)"),
    ("kill tag:<name>", "Kill all with tag"),
    ("kill all", "Kill all with tracked PIDs"),
    ("", ""),
];

const LISTEN_HELP: &[HelpEntry] = &[
    ("listen [timeout]", "Block until message arrives"),
    ("  [timeout]", "Timeout in seconds (alias for --timeout)"),
    ("  --timeout N", "Timeout in seconds (default: 86400)"),
    ("  --json", "Output messages as JSON"),
    ("", ""),
    ("Filter flags:", ""),
    ("", "Supports all filter flags from 'events' command"),
    (
        "",
        "(--agent, --type, --status, --file, --cmd, --from, --intent, etc.)",
    ),
    ("", "Run 'hcom events --help' for full list"),
    ("", "Filters combine with --sql using AND logic"),
    ("", ""),
    ("SQL filter mode:", ""),
    ("  --sql \"type='message'\"", "Custom SQL against events_v"),
    ("  --sql stopped:name", "Preset: wait for agent to stop"),
    ("  --idle NAME", "Shortcut: wait for agent to go idle"),
    ("", ""),
    ("Exit codes:", ""),
    ("  0", "Message received / event matched"),
    ("  1", "Timeout or error"),
    ("", ""),
    ("", "Quick unread check: hcom listen 1"),
];

const RESET_HELP: &[HelpEntry] = &[
    ("reset", "Archive conversation, clear database"),
    (
        "reset all",
        "Stop all + clear db + remove hooks + reset config",
    ),
    ("", ""),
    ("Sandbox / local mode:", ""),
    ("", "If you can't write to ~/.hcom, set:"),
    ("", "  export HCOM_DIR=\"$PWD/.hcom\""),
    (
        "",
        "Hooks install under $PWD (.claude/.gemini/.codex) or ~/.config/opencode/, state in $HCOM_DIR",
    ),
    ("", ""),
    ("", "To remove local setup:"),
    ("", "  hcom hooks remove && rm -rf \"$HCOM_DIR\""),
    ("", ""),
    ("", "Explicit location:"),
    ("", "  export HCOM_DIR=/your/path/.hcom"),
    ("", ""),
];

const CONFIG_HELP: &[HelpEntry] = &[
    ("config", "Show effective config (with sources)"),
    ("config <key>", "Get one key"),
    ("config <key> <value>", "Set one key"),
    ("config <key> --info", "Detailed help for a key"),
    ("  --json / --edit / --reset", ""),
    ("", ""),
    ("Per-agent:", ""),
    (
        "config -i <name|self> [key] [val]",
        "tag, timeout, hints, subagent_timeout",
    ),
    ("", ""),
    ("Keys:", ""),
    ("  tag", "Group/label (agents become tag-*)"),
    ("  terminal", "Where new agent windows open"),
    ("  hints", "Text appended to all messages agent receives"),
    ("  notes", "Notes appended to agent bootstrap"),
    (
        "  subagent_timeout",
        "Subagent keep-alive seconds after task",
    ),
    (
        "  claude_args / gemini_args / codex_args / opencode_args",
        "",
    ),
    ("  auto_approve", "Auto-approve safe hcom commands"),
    ("  auto_subscribe", "Event auto-subscribe presets"),
    ("  name_export", "Export agent name to custom env var"),
    ("", "hcom config <key> --info for details"),
    ("", ""),
    ("", "Precedence: defaults < config.toml < env vars"),
];

// config help continued with dynamic config files hint
const CONFIG_HELP_2: &[HelpEntry] = &[(
    "",
    "HCOM_DIR: isolate per project (see 'hcom reset --help')",
)];

const RELAY_HELP: &[HelpEntry] = &[
    ("relay", "Show status and token"),
    ("relay new", "Create new relay group"),
    ("relay connect <token>", "Join relay group"),
    ("relay on / off", "Enable/disable sync"),
    ("", ""),
    ("Setup:", ""),
    ("", "1. 'relay new' to get token"),
    ("", "2. 'relay connect <token>' on each device"),
    ("", ""),
    ("Custom broker:", ""),
    (
        "relay new --broker mqtts://host:port --password <broker-auth-secret>",
        "",
    ),
    ("relay connect <token> --password <secret>", ""),
    ("", ""),
    ("Daemon:", ""),
    ("relay daemon", "Show daemon status"),
    ("relay daemon start", "Start the relay daemon"),
    ("relay daemon stop", "Stop the daemon (SIGKILL after 5s)"),
    ("relay daemon restart", "Restart the daemon"),
    ("", ""),
];

const TRANSCRIPT_HELP: &[HelpEntry] = &[
    ("transcript <name>", "View agent's conversation (last 10)"),
    ("transcript <name> N", "Show exchange N"),
    ("transcript <name> N-M", "Show exchanges N through M"),
    (
        "transcript timeline",
        "User prompts across all agents by time",
    ),
    ("  --last N", "Limit to last N exchanges (default: 10)"),
    ("  --full", "Show complete assistant responses"),
    ("  --detailed", "Show tool I/O, file edits, errors"),
    ("  --json", "JSON output"),
    ("", ""),
    (
        "transcript search \"pattern\"",
        "Search hcom-tracked transcripts (rg/grep)",
    ),
    ("  --live", "Only currently alive agents"),
    ("  --all", "All transcripts (includes non-hcom sessions)"),
    ("  --limit N", "Max results (default: 20)"),
    (
        "  --agent TYPE",
        "Filter: claude | gemini | codex | opencode",
    ),
    (
        "  --exclude-self",
        "Exclude the searching agent's own transcript",
    ),
    ("  --json", "JSON output"),
    ("", ""),
    ("", "Tip: Reference ranges in messages instead of copying:"),
    ("", "\"read my transcript range 7-10 --full\""),
];

const ARCHIVE_HELP: &[HelpEntry] = &[
    ("archive", "List archived sessions (numbered)"),
    ("archive <N>", "Query events from archive (1 = most recent)"),
    ("archive <N> agents", "Query agents from archive"),
    ("archive <name>", "Query by stable name (prefix match)"),
    ("  --here", "Filter to archives from current directory"),
    ("  --sql \"expr\"", "SQL WHERE filter"),
    ("  --last N", "Limit events (default: 20)"),
    ("  --json", "JSON output"),
];

const RUN_HELP: &[HelpEntry] = &[
    (
        "run",
        "List available workflow/launch scripts and more info",
    ),
    ("run <name> [args]", "Execute script"),
    ("run <name> --help", "Script options"),
    ("run docs", "CLI reference + config + script creation guide"),
    ("", ""),
    ("", "Docs sections:"),
    ("  hcom run docs --cli", "CLI reference only"),
    ("  hcom run docs --config", "Config settings only"),
    ("  hcom run docs --scripts", "Script creation guide"),
    ("", ""),
    ("", "User scripts: ~/.hcom/scripts/"),
];

const STATUS_HELP: &[HelpEntry] = &[
    ("status", "Installation status and diagnostics"),
    ("status --logs", "Include recent errors and warnings"),
    ("status --json", "Machine-readable output"),
];

const UPDATE_HELP: &[HelpEntry] = &[
    ("update", "Check for and apply updates"),
    (
        "update --check",
        "Only check — print status without applying",
    ),
    ("", ""),
    (
        "",
        "Detects install method and runs the right update command:",
    ),
    ("", "  brew install    → brew upgrade hcom"),
    ("", "  uv tool install → uv tool upgrade hcom"),
    ("", "  pip install     → pip install -U hcom"),
    ("", "  curl installer  → re-run hcom-installer.sh"),
];

const HOOKS_HELP: &[HelpEntry] = &[
    ("hooks", "Show hook status"),
    ("hooks status", "Same as above"),
    (
        "hooks add [tool]",
        "Add hooks (claude | gemini | codex | opencode | all)",
    ),
    (
        "hooks remove [tool]",
        "Remove hooks (claude | gemini | codex | opencode | all)",
    ),
    ("", ""),
    (
        "",
        "Hooks enable automatic message delivery and status tracking.",
    ),
    (
        "",
        "Without hooks, use ad-hoc mode (run hcom start inside any AI tool).",
    ),
    ("", "Restart the tool after adding hooks to activate."),
    (
        "",
        "Remove cleans both global (~/) and HCOM_DIR-local if set.",
    ),
];

const TERM_HELP: &[HelpEntry] = &[
    ("term", "Screen dump (all PTY instances)"),
    ("term [name]", "Screen dump for specific agent"),
    ("  --json", "Raw JSON output"),
    ("", ""),
    ("term inject <name> [text]", "Inject text into agent PTY"),
    (
        "  --enter",
        "Append \\r (submit). Works alone or with text.",
    ),
    ("", ""),
    ("term debug on", "Enable PTY debug logging (all instances)"),
    ("term debug off", "Disable PTY debug logging"),
    ("term debug logs", "List debug log files"),
    ("", ""),
    ("JSON fields:", "lines[], size[rows,cols], cursor[row,col],"),
    ("", "ready, prompt_empty, input_text"),
    ("", ""),
    ("", "Debug toggle; instances detect within ~10s."),
    ("", "Logs: ~/.hcom/.tmp/logs/pty_debug/"),
];

// ── Tool launch help (claude/gemini/codex/opencode) ─────────────────────

/// Per-tool differences for launch help generation.
struct ToolHelpSpec {
    name: &'static str,
    label: &'static str,
    /// Tool-specific examples (forwarded flags, etc.)
    unique_examples: &'static [HelpEntry],
    /// Extra env vars beyond the shared set.
    extra_env: &'static [HelpEntry],
    has_fork: bool,
}

const CLAUDE_SPEC: ToolHelpSpec = ToolHelpSpec {
    name: "claude",
    label: "Claude",
    unique_examples: &[
        ("hcom 3 claude -p \"prompt\"", "3 headless in background"),
        ("hcom 1 claude --agent <name>", ".claude/agents/<name>.md"),
    ],
    extra_env: &[(
        "HCOM_SUBAGENT_TIMEOUT",
        "Seconds subagents keep-alive after task",
    )],
    has_fork: true,
};

const GEMINI_SPEC: ToolHelpSpec = ToolHelpSpec {
    name: "gemini",
    label: "Gemini",
    unique_examples: &[("hcom N gemini --yolo", "Flags forwarded to gemini")],
    extra_env: &[("HCOM_GEMINI_SYSTEM_PROMPT", "System prompt (env var)")],
    has_fork: false,
};

const CODEX_SPEC: ToolHelpSpec = ToolHelpSpec {
    name: "codex",
    label: "Codex",
    unique_examples: &[(
        "hcom codex --sandbox danger-full-access",
        "Flags forwarded to codex",
    )],
    extra_env: &[(
        "HCOM_CODEX_SYSTEM_PROMPT",
        "System prompt (env var or config)",
    )],
    has_fork: true,
};

const OPENCODE_SPEC: ToolHelpSpec = ToolHelpSpec {
    name: "opencode",
    label: "OpenCode",
    unique_examples: &[],
    extra_env: &[],
    has_fork: true,
};

fn get_tool_spec(name: &str) -> Option<&'static ToolHelpSpec> {
    match name {
        "claude" => Some(&CLAUDE_SPEC),
        "gemini" => Some(&GEMINI_SPEC),
        "codex" => Some(&CODEX_SPEC),
        "opencode" => Some(&OPENCODE_SPEC),
        _ => None,
    }
}

/// Generate tool launch help from spec. Returns formatted lines (not HelpEntry).
fn generate_tool_help(spec: &ToolHelpSpec) -> String {
    let t = spec.name;
    let inside_ai = crate::shared::is_inside_ai_tool();
    let term_desc = if inside_ai {
        "Opens new terminal"
    } else {
        "Runs in current terminal"
    };
    let args_env = format!("HCOM_{}_ARGS", t.to_uppercase());

    let mut lines: Vec<String> = Vec::new();

    // Usage + examples
    lines.push("Usage:".to_string());
    lines.push(format!(
        "  hcom [N] {} [args...]       Launch N {} agents (default N=1)",
        t, spec.label
    ));
    lines.push(String::new());
    // Example block — all at same indent level using format helper
    let ex = |usage: &str, desc: &str| -> String { format!("    {:<34} {}", usage, desc) };
    lines.push(ex(&format!("hcom {}", t), term_desc));
    lines.push(ex(&format!("hcom 3 {}", t), "Opens 3 new terminal windows"));
    for (u, d) in spec.unique_examples {
        lines.push(ex(u, d));
    }

    // hcom flags
    lines.push(String::new());
    lines.push("hcom Flags:".to_string());
    lines.push("    --tag <name>                 Group tag (names become tag-*)".to_string());
    lines.push("    --terminal <preset>          Where new windows open".to_string());
    lines.push("    --hcom-prompt <text>          Initial prompt".to_string());
    lines.push("    --hcom-system-prompt <text>   System prompt".to_string());

    // Environment
    lines.push(String::new());
    lines.push("Environment:".to_string());
    lines.push(format!(
        "    {:<28} Default args (merged with CLI)",
        args_env
    ));
    lines.push(format!(
        "    {:<28} Group tag (agents become tag-*)",
        "HCOM_TAG"
    ));
    lines.push(format!(
        "    {:<28} default | <preset> | \"cmd {{script}}\"",
        "HCOM_TERMINAL"
    ));
    lines.push(format!(
        "    {:<28} Appended to messages received",
        "HCOM_HINTS"
    ));
    lines.push(format!("    {:<28} One-time bootstrap notes", "HCOM_NOTES"));
    for (u, d) in spec.extra_env {
        lines.push(format!("    {:<28} {}", u.trim(), d));
    }

    // Resume / Fork
    lines.push(String::new());
    if spec.has_fork {
        lines.push("Resume / Fork:".to_string());
        lines.push("    hcom r <name>                  Resume stopped agent by name".to_string());
        lines.push(
            "    hcom f <name>                  Fork agent session (active or stopped)".to_string(),
        );
    } else {
        lines.push("Resume:".to_string());
        lines.push("    hcom r <name>                  Resume stopped agent by name".to_string());
        lines.push(format!(
            "  {} does not support session forking (hcom f).",
            spec.label
        ));
    }

    // Footer
    lines.push(String::new());
    lines.push(format!("  Run \"{} --help\" for {} options.", t, t));
    lines.push("  Run \"hcom config terminal --info\" for terminal presets.".to_string());

    lines.join("\n")
}

// ── Format a single help entry ──────────────────────────────────────────

fn format_entry(usage: &str, desc: &str) -> String {
    if usage.is_empty() {
        // Empty usage: plain text or blank line
        if desc.is_empty() {
            String::new()
        } else {
            format!("  {}", desc)
        }
    } else if usage.starts_with("  ") {
        // Indented: option/setting line
        format!("  {:<32} {}", usage, desc)
    } else if usage.ends_with(':') {
        // Section header
        if desc.is_empty() {
            format!("\n{}", usage)
        } else {
            format!("\n{} {}", usage, desc)
        }
    } else {
        // Command line
        format!("  hcom {:<26} {}", usage, desc)
    }
}

/// Format entries from a static slice.
fn format_entries(entries: &[HelpEntry]) -> Vec<String> {
    entries.iter().map(|(u, d)| format_entry(u, d)).collect()
}

// ── Lookup and render ───────────────────────────────────────────────────

/// Ordered list of all commands (for docs generation).
pub const COMMAND_NAMES: &[&str] = &[
    "send",
    "list",
    "events",
    "stop",
    "start",
    "listen",
    "status",
    "config",
    "hooks",
    "archive",
    "reset",
    "transcript",
    "bundle",
    "kill",
    "term",
    "relay",
    "run",
    "update",
    "claude",
    "gemini",
    "codex",
    "opencode",
];

/// Get the top-level help text as a String.
pub fn get_help_text() -> String {
    format!(
        "hcom (hook-comms) v{} - multi-agent communication\n\
\n\
Usage:\n\
  hcom                                  TUI dashboard\n\
  hcom <command>                        Run command\n\
\n\
Launch:\n\
  hcom [N] claude|gemini|codex|opencode [flags] [tool-args]\n\
  hcom r <name>                         Resume stopped agent\n\
  hcom f <name>                         Fork agent session\n\
  hcom kill <name(s)|tag:T|all>         Kill + close terminal pane\n\
\n\
Commands:\n\
  send         Send message to your buddies\n\
  listen       Block until message or event arrives\n\
  list         Show agents, status, unread counts\n\
  events       Query event stream, manage subscriptions\n\
  bundle       Structured context packages for handoffs\n\
  transcript   Read another agent's conversation\n\
  start        Connect to hcom (run inside any AI tool)\n\
  stop         Disconnect from hcom\n\
  config       Get/set global and per-agent settings\n\
  run          Execute workflow scripts\n\
  relay        Cross-device sync + relay daemon\n\
  archive      Query past hcom sessions\n\
  reset        Archive and clear database\n\
  hooks        Add or remove hooks\n\
  status       Installation and diagnostics\n\
  term         View/inject into agent PTY screens\n\
  update       Check and apply updates",
        env!("CARGO_PKG_VERSION"),
    )
}

/// Get formatted help for a single command.
pub fn get_command_help(name: &str) -> String {
    let mut lines = vec!["Usage:".to_string()];

    // Tool launch commands use the template generator
    if let Some(spec) = get_tool_spec(name) {
        return generate_tool_help(spec);
    }

    let entries: Option<&[HelpEntry]> = match name {
        "list" => Some(LIST_HELP),
        "send" => Some(SEND_HELP),
        "bundle" => Some(BUNDLE_HELP),
        "stop" => Some(STOP_HELP),
        "start" => Some(START_HELP),
        "kill" => Some(KILL_HELP),
        "listen" => Some(LISTEN_HELP),
        "reset" => Some(RESET_HELP),
        "relay" => Some(RELAY_HELP),
        "transcript" => Some(TRANSCRIPT_HELP),
        "archive" => Some(ARCHIVE_HELP),
        "run" => Some(RUN_HELP),
        "status" => Some(STATUS_HELP),
        "update" => Some(UPDATE_HELP),
        "hooks" => Some(HOOKS_HELP),
        "term" => Some(TERM_HELP),
        _ => None,
    };

    // Events is special: spliced with FILTER_HELP in the middle
    if name == "events" || name == "events sub" {
        lines.extend(format_entries(EVENTS_HELP));
        lines.push(String::new()); // blank before filters header
        lines.push(String::from(
            "\nFilters (same flag repeated = OR, different flags = AND):",
        ));
        lines.extend(format_entries(FILTER_HELP));
        lines.extend(format_entries(EVENTS_HELP_2));
        return lines.join("\n");
    }

    // Config is special: has dynamic config files hint
    if name == "config" {
        lines.extend(format_entries(CONFIG_HELP));
        // Dynamic: resolved config file paths
        let hcom_dir = env::var("HCOM_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".hcom"));
        let config_toml = hcom_dir.join("config.toml");
        let env_file = hcom_dir.join("config.env");
        lines.push(format!(
            "  Files: {}, {}",
            config_toml.display(),
            env_file.display()
        ));
        lines.extend(format_entries(CONFIG_HELP_2));
        return lines.join("\n");
    }

    if let Some(entries) = entries {
        lines.extend(format_entries(entries));
        lines.join("\n")
    } else {
        // Try parent command (e.g. "events sub" -> "events")
        if let Some(pos) = name.rfind(' ') {
            let parent = &name[..pos];
            if parent != name {
                return get_command_help(parent);
            }
        }
        format!("Usage: hcom {}", name)
    }
}

/// Print help for a command to stdout.
pub fn print_command_help(name: &str) {
    println!("{}", get_command_help(name));
}

pub fn print_help() {
    println!("{}", get_help_text());
    println!();
    println!("Identity:");
    println!("  1. Run hcom start to get a name");
    println!("  2. Use --name <name> on all hcom commands");
    println!();
    println!("Run 'hcom <command> --help' for details.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_text_contains_version() {
        // Capture what print_help would output by checking the format string
        let version = env!("CARGO_PKG_VERSION");
        assert!(!version.is_empty());
    }

    #[test]
    fn all_commands_have_help() {
        let commands = [
            "send",
            "list",
            "events",
            "stop",
            "start",
            "listen",
            "status",
            "config",
            "hooks",
            "archive",
            "reset",
            "transcript",
            "bundle",
            "kill",
            "term",
            "relay",
            "run",
            "claude",
            "gemini",
            "codex",
            "opencode",
        ];
        for cmd in commands {
            let help = get_command_help(cmd);
            assert!(
                help.starts_with("Usage:"),
                "help for '{}' should start with 'Usage:'",
                cmd
            );
            assert!(help.len() > 20, "help for '{}' should have content", cmd);
        }
    }

    #[test]
    fn unknown_command_fallback() {
        let help = get_command_help("nonexistent");
        assert_eq!(help, "Usage: hcom nonexistent");
    }

    #[test]
    fn events_sub_resolves_to_events() {
        let help = get_command_help("events sub");
        assert!(
            help.contains("Subscribe"),
            "events sub help should contain Subscribe section"
        );
    }

    #[test]
    fn format_entry_rules() {
        // Blank line
        assert_eq!(format_entry("", ""), "");
        // Plain text
        assert_eq!(format_entry("", "some text"), "  some text");
        // Option line (indented)
        assert!(format_entry("  --json", "Output JSON").contains("--json"));
        // Section header
        assert!(format_entry("Examples:", "").starts_with('\n'));
        // Command line
        assert!(format_entry("list", "Show agents").contains("hcom list"));
    }
}
