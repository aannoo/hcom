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
        "created | started | ready | stopped | batch_launched | launch_failed | launch_blocked",
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
    (
        "  --participant NAME",
        "Message sender or delivery recipient",
    ),
    ("  --mention NAME", "@mention target"),
    ("  --intent VAL", "request | inform | ack"),
    ("  --thread NAME", "Thread name"),
    ("  --after TIME", "After timestamp (ISO-8601)"),
    ("  --before TIME", "Before timestamp (ISO-8601)"),
];

// ── Per-command help registries ─────────────────────────────────────────
//
// A command page is a list of sections; a subcommand page (`SUBCOMMAND_HELP`)
// reuses the same sections, so `hcom X --help` and `hcom X sub --help` can't
// drift apart.

const FILTERS_HEADER: &[HelpEntry] = &[(
    "Filters (same flag repeated = OR, different flags = AND):",
    "",
)];

const EVENTS_QUERY: &[HelpEntry] = &[
    (
        "",
        "Query the event stream (messages, status changes, file edits, lifecycle)",
    ),
    ("Query:", ""),
    (
        "  events",
        "Last 20 events as JSON Lines (one object per line)",
    ),
    ("  --last N", "Limit count (default: 20)"),
    ("  --all", "Include archived sessions"),
    ("  --full", "Full event JSON (default: streamlined)"),
    ("  --wait [SEC]", "Block until match (default: 60s)"),
    ("  --sql EXPR", "Raw SQL WHERE (ANDed with flags)"),
    (
        "  --remote-fetch --device ID",
        "One-shot fetch from remote device",
    ),
];

const EVENTS_SHORTCUTS: &[HelpEntry] = &[
    ("Shortcuts:", ""),
    ("  --idle NAME", "--agent NAME --status listening"),
    ("  --blocked NAME", "--agent NAME --status blocked"),
];

const EVENTS_LAUNCH: &[HelpEntry] = &[
    ("Wait for a launch batch:", ""),
    (
        "events launch [batch_id]",
        "Block until batch reaches a terminal state (JSON LaunchResult)",
    ),
    ("  --timeout SEC", "Max seconds to wait (default: 30)"),
    (
        "",
        "Exit codes: 0 ready, 1 error/no_launches, 2 timeout/blocked",
    ),
];

const EVENTS_SUB: &[HelpEntry] = &[
    (
        "Subscribe (next matching event arrives as a message from [hcom-events]):",
        "",
    ),
    ("events sub list", "List active subscriptions"),
    ("events sub [filters]", "Subscribe using filter flags"),
    ("events sub \"SQL WHERE\"", "Subscribe using raw SQL"),
    ("  --once", "Auto-remove after first match"),
    ("  --for <name>", "Subscribe on behalf of another agent"),
    (
        "  --on-hit <text>",
        "Also send this message each time it fires (@mentions ok)",
    ),
    (
        "  --as <name> / -b",
        "Own the sub without an agent identity, e.g. from a shell (-b = bigboss)",
    ),
    (
        "  --device ID",
        "Install/list on a remote device (create requires --for, --as, or -b)",
    ),
];

const EVENTS_UNSUB: &[HelpEntry] = &[
    ("events unsub <id>", "Remove a subscription"),
    ("  --device ID", "Remove it from a remote device"),
];

const EXAMPLES_HEADER: &[HelpEntry] = &[("Examples:", "")];

const EVENTS_EXAMPLES: &[HelpEntry] = &[("  hcom events --cmd git --agent peso", "")];

const EVENTS_SUB_EXAMPLES: &[HelpEntry] = &[
    (
        "  hcom events sub --idle peso",
        "Notified when peso goes idle",
    ),
    (
        "  hcom events sub --file '*.py' --once",
        "One-shot: next .py file write",
    ),
];

const EVENTS_SQL_REFERENCE: &[HelpEntry] = &[
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
        "created, started, ready, stopped, batch_launched, launch_failed, launch_blocked",
    ),
    ("", ""),
    (
        "",
        "delivered_to/mentions are JSON arrays \u{2014} query exact values with json_each(...)",
    ),
    ("", "Use <> instead of != for SQL negation"),
];

const EVENTS_SQL_POINTER: &[HelpEntry] = &[("", ""), ("", "SQL columns: hcom events --help")];

const EVENTS_PAGE: &[&[HelpEntry]] = &[
    EVENTS_QUERY,
    FILTERS_HEADER,
    FILTER_HELP,
    EVENTS_SHORTCUTS,
    EVENTS_LAUNCH,
    EVENTS_SUB,
    EVENTS_UNSUB,
    EXAMPLES_HEADER,
    EVENTS_EXAMPLES,
    EVENTS_SUB_EXAMPLES,
    EVENTS_SQL_REFERENCE,
];

const EVENTS_SUB_PAGE: &[&[HelpEntry]] = &[
    EVENTS_SUB,
    FILTERS_HEADER,
    FILTER_HELP,
    EVENTS_SHORTCUTS,
    EXAMPLES_HEADER,
    EVENTS_SUB_EXAMPLES,
    EVENTS_SQL_POINTER,
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
        "[CLAUDE] [GEMINI] [CODEX] [OPENCODE] [KILO] [PI] [OMP] [ANTIGRAVITY] [CURSOR] [KIMI] [COPILOT] [GROK]  hcom-launched, automatic delivery",
    ),
    (
        "",
        "[CLAUDE*]   hooks or process not bound (starting up, or lost); see: list <name>",
    ),
    ("", "[AD-HOC]    no hooks, checks messages manually"),
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
    ("  @luna", "exact base name"),
    ("  @api-luna", "exact full name"),
    ("  @api-", "all local agents with exact tag 'api'"),
    ("  @luna:BOXE", "exact or uniquely prefixed remote agent"),
    (
        "",
        "Partial local names are rejected to avoid accidental fan-out.",
    ),
    ("", ""),
    ("Envelope:", ""),
    ("  --intent <type>", "request | inform | ack"),
    ("", "  request: expect a response"),
    ("", "  inform: FYI, no response needed"),
    ("", "  ack: replying to a request (requires --reply-to)"),
    ("  --reply-to <id>", "Link to event ID (42 or 42:BOXE)"),
    (
        "  --thread <name>",
        "Threaded routing: seed recipients once, then reuse thread members",
    ),
    (
        "",
        "  broadcast + --thread reuses prior thread members; seed with @mentions first",
    ),
    ("", ""),
    ("Sender:", ""),
    ("  --from <name>", "External sender identity"),
    ("  -b", "Shorthand for --from bigboss"),
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

const BLANK: &[HelpEntry] = &[("", "")];

const BUNDLE_DEFAULT: &[HelpEntry] = &[("bundle", "List recent bundles (same as bundle list)")];

const BUNDLE_LIST: &[HelpEntry] = &[
    ("bundle list", "List recent bundles"),
    ("  --last N", "Limit count (default: 20)"),
    ("  --json", "Output JSON"),
];

const BUNDLE_CAT: &[HelpEntry] = &[
    ("bundle cat <id>", "Expand full bundle content"),
    (
        "",
        "Shows: metadata, files (metadata only), transcript (respects detail level), events",
    ),
];

const BUNDLE_PREPARE: &[HelpEntry] = &[
    ("bundle prepare", "Show recent context, suggest template"),
    (
        "  --for <agent>",
        "Prepare for specific agent (default: self)",
    ),
    (
        "  --last-transcript N",
        "Transcript exchanges to suggest (default: 40)",
    ),
    (
        "  --last-events N",
        "Events to scan per category (default: 10)",
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
];

const BUNDLE_SHOW: &[HelpEntry] = &[
    ("bundle show <id>", "Show bundle by id/prefix"),
    ("  --json", "Output JSON"),
];

const BUNDLE_CREATE: &[HelpEntry] = &[
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
        "    normal = truncated | full = complete text | detailed = tool I/O+edits+errors",
    ),
    ("  --extends <id>", "Parent bundle for chaining"),
    ("  --bundle JSON", "Create from JSON payload (format below)"),
    (
        "  --bundle-file FILE",
        "Create from JSON file (format below)",
    ),
    ("  --json", "Output JSON"),
];

const BUNDLE_CHAIN: &[HelpEntry] = &[
    ("bundle chain <id>", "Show bundle lineage"),
    ("  --json", "Output JSON"),
];

const BUNDLE_JSON_FORMAT: &[HelpEntry] = &[
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
];

const BUNDLE_PAGE: &[&[HelpEntry]] = &[
    BUNDLE_DEFAULT,
    BUNDLE_LIST,
    BLANK,
    BUNDLE_CAT,
    BLANK,
    BUNDLE_PREPARE,
    BLANK,
    BUNDLE_SHOW,
    BLANK,
    BUNDLE_CREATE,
    BLANK,
    BUNDLE_CHAIN,
    BUNDLE_JSON_FORMAT,
];

const STOP_HELP: &[HelpEntry] = &[
    ("stop", "Disconnect self from hcom"),
    ("stop <name>", "Disconnect specific agent"),
    ("stop <n1> <n2> ...", "Disconnect multiple"),
    ("stop tag:<name>", "Disconnect all with tag"),
    ("stop all", "Disconnect all agents"),
];

const START_HELP: &[HelpEntry] = &[
    ("start", "Connect to hcom (from inside any AI session)"),
    (
        "start --name <agent-id>",
        "Register a subagent using its agent ID (from SubagentStart)",
    ),
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
    (
        "kill <name>...",
        "Kill one or more processes (+ close terminal panes)",
    ),
    ("kill tag:<name>", "Kill all with tag"),
    ("kill all", "Kill all with tracked PIDs"),
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
        "Hooks install under the parent of HCOM_DIR; state stays in HCOM_DIR.",
    ),
    (
        "",
        "  HCOM_DIR=$PWD/.hcom -> $PWD/.claude, .gemini, .codex, .opencode, .kilo, .pi, .omp, .antigravity, .cursor, .kimi, .copilot, .grok",
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
    (
        "  --json / --edit / --reset",
        "JSON output / open in $EDITOR / archive + restore defaults",
    ),
    ("", ""),
    ("Per-agent:", ""),
    (
        "config -i <name|self> [key] [val]",
        "tag, timeout, hints, subagent_timeout",
    ),
];

/// Keys a user sets by hand. Every CONFIG_KEYS entry must appear here or in
/// CONFIG_KEYS_NOT_IN_HELP (enforced by `config_help_covers_registry_keys`).
const CONFIG_KEYS_HELP: &[HelpEntry] = &[
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
        "  claude_args / gemini_args / codex_args / opencode_args / kilo_args / pi_args / omp_args / cursor_args / kimi_args / copilot_args / grok_args",
        "Default launch args per tool",
    ),
    (
        "  gemini_system_prompt / codex_system_prompt",
        "Default system prompt",
    ),
    (
        "  codex_sandbox_mode",
        "workspace | danger-full-access | none",
    ),
    ("  auto_approve", "Auto-approve safe hcom commands"),
    ("  auto_subscribe", "Event auto-subscribe presets"),
    (
        "  auto_trust_workspace",
        "Auto-trust launch dir (skip folder-trust prompt)",
    ),
    ("  name_export", "Export agent name to custom env var"),
    (
        "  title_mode",
        "Terminal/tab title: combined, label, or off",
    ),
];

/// Registry keys left out of help: relay keys are managed by `hcom relay`;
/// `timeout` only affects headless/vanilla Claude (see `config timeout --info`).
#[cfg(test)]
const CONFIG_KEYS_NOT_IN_HELP: &[&str] = &[
    "relay",
    "relay_id",
    "relay_token",
    "relay_enabled",
    "timeout",
];

// config help continued with dynamic config files hint
const CONFIG_HELP_2: &[HelpEntry] = &[(
    "",
    "HCOM_DIR: isolate per project (see 'hcom reset --help')",
)];

const RELAY_MAIN: &[HelpEntry] = &[
    ("relay", "Show relay status"),
    ("relay new", "Create new relay group"),
    ("relay token", "Show join token for other devices"),
    ("relay connect <token>", "Join relay group"),
    ("relay on", "Re-enable sync"),
    ("relay off", "Disable sync"),
    ("  --all", "Also disable on all known peers"),
    ("Setup:", ""),
    ("", "1. 'relay new' to get token"),
    ("", "2. 'relay connect <token>' on each device"),
    ("Custom broker:", ""),
    (
        "relay new --broker mqtts://host:port --password <broker-auth-secret>",
        "",
    ),
    ("relay connect <token> --password <broker-auth-secret>", ""),
    ("Daemon:", ""),
];

const RELAY_DAEMON: &[HelpEntry] = &[
    ("relay daemon", "Show daemon status"),
    ("relay daemon start", "Start the relay daemon"),
    ("relay daemon stop", "Stop the daemon (SIGKILL after 5s)"),
    ("relay daemon restart", "Restart the daemon"),
];

const RELAY_PAGE: &[&[HelpEntry]] = &[RELAY_MAIN, RELAY_DAEMON];

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
    ("  --agent TYPE", "Filter: {transcript_agents}"),
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
    (
        "  --last N",
        "Max archives listed (default: all); events when one is selected (default: 20)",
    ),
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

const HOOKS_STATUS: &[HelpEntry] = &[
    ("hooks", "Show how each tool loads hcom's hooks"),
    ("hooks status", "Same as above"),
];

const HOOKS_ADD: &[HelpEntry] = &[
    (
        "hooks add [tool]",
        "Install persistent hooks ({persistent_tools} | all)",
    ),
    (
        "",
        "  No tool: auto-detect the current tool, or add all outside one.",
    ),
    ("", "  Restart the tool after adding hooks to activate."),
];

const HOOKS_REMOVE: &[HelpEntry] = &[
    (
        "hooks remove [tool]",
        "Remove hooks and legacy installs ({hook_tools} | all)",
    ),
    ("", "  No tool: remove from all tools."),
    (
        "",
        "  Cleans both global (~/) and HCOM_DIR-local hooks if set.",
    ),
];

const HOOKS_ABOUT: &[HelpEntry] = &[
    ("", "Per-run: {per_run_tools}"),
    (
        "",
        "  Hooks load only in sessions launched with `hcom <tool>`. Nothing to",
    ),
    (
        "",
        "  install; remove clears installs left by older hcom versions.",
    ),
    ("", "Persistent: {persistent_list}"),
    (
        "",
        "  Hooks live in the tool's config; install them with `hooks add`.",
    ),
    ("", "No hooks: {hookless_list}"),
    (
        "",
        "  `hcom <tool>` gets status and messages over the tool's own connection.",
    ),
    ("", ""),
    (
        "",
        "Without hooks, run `hcom start` inside any AI tool (manual delivery).",
    ),
];

const HOOKS_PAGE: &[&[HelpEntry]] = &[HOOKS_STATUS, HOOKS_ADD, HOOKS_REMOVE, BLANK, HOOKS_ABOUT];

const TERM_SCREEN: &[HelpEntry] = &[
    ("term", "Screen dump (all PTY instances)"),
    ("term [name]", "Screen dump for specific agent"),
    ("  --json", "Raw JSON output"),
    (
        "",
        "    Fields: lines[], size[rows,cols], cursor[row,col], ready, prompt_empty, input_text",
    ),
    (
        "  --clean",
        "No status header or line numbers ([name] labels kept for all)",
    ),
];

const TERM_INJECT: &[HelpEntry] = &[
    ("term inject <name> [text]", "Inject text into agent PTY"),
    (
        "  --enter",
        "Append \\r (submit). Works alone or with text.",
    ),
];

const TERM_DEBUG: &[HelpEntry] = &[
    ("term debug on", "Enable PTY debug logging (all instances)"),
    ("term debug off", "Disable PTY debug logging"),
    ("term debug logs", "List debug log files"),
    ("", "Instances pick up the toggle within ~10s."),
    ("", "Logs: ~/.hcom/.tmp/logs/pty_debug/"),
];

const TERM_PAGE: &[&[HelpEntry]] = &[TERM_SCREEN, BLANK, TERM_INJECT, BLANK, TERM_DEBUG];

/// Subcommands with their own `--help` page, built from their parent's sections.
const SUBCOMMAND_HELP: &[(&str, &[&[HelpEntry]])] = &[
    ("events sub", EVENTS_SUB_PAGE),
    ("events unsub", &[EVENTS_UNSUB]),
    ("events launch", &[EVENTS_LAUNCH]),
    ("bundle list", &[BUNDLE_LIST]),
    ("bundle show", &[BUNDLE_SHOW]),
    ("bundle cat", &[BUNDLE_CAT]),
    ("bundle chain", &[BUNDLE_CHAIN]),
    ("bundle prepare", &[BUNDLE_PREPARE]),
    ("bundle create", &[BUNDLE_CREATE, BUNDLE_JSON_FORMAT]),
    ("relay daemon", &[RELAY_DAEMON]),
    ("hooks add", &[HOOKS_ADD]),
    ("hooks remove", &[HOOKS_REMOVE]),
    ("term inject", &[TERM_INJECT]),
    ("term debug", &[TERM_DEBUG]),
];

/// Help topic for `hcom <cmd> <args...> --help`: `"<cmd> <sub>"` when the first
/// arg is a subcommand with its own page, otherwise just `cmd`.
pub fn help_topic(cmd: &str, args: &[String]) -> String {
    if let Some(sub) = args.first() {
        let sub = match (cmd, sub.as_str()) {
            ("bundle", "preview") => "prepare",
            ("hooks", "install") => "add",
            ("hooks", "uninstall") => "remove",
            (_, sub) => sub,
        };
        let topic = format!("{cmd} {sub}");
        if SUBCOMMAND_HELP.iter().any(|(t, _)| *t == topic) {
            return topic;
        }
    }
    cmd.to_string()
}

/// Fill hook-tool placeholders from released integrations.
fn expand_hook_tools(text: &str) -> String {
    use crate::hooks::runtime::HookMode;
    let tools = crate::commands::hooks::hook_tools();
    let names = |keep: &dyn Fn(HookMode) -> bool, sep: &str| {
        tools
            .iter()
            .filter(|tool| keep(HookMode::of(**tool)))
            .map(|tool| tool.as_str())
            .collect::<Vec<_>>()
            .join(sep)
    };
    text.replace("{hook_tools}", &names(&|m| m != HookMode::None, " | "))
        .replace(
            "{persistent_tools}",
            &names(&|m| m == HookMode::Persistent, " | "),
        )
        .replace(
            "{persistent_list}",
            &names(&|m| m == HookMode::Persistent, ", "),
        )
        .replace("{per_run_tools}", &names(&|m| m == HookMode::PerRun, ", "))
        .replace("{hookless_list}", &names(&|m| m == HookMode::None, ", "))
}

// ── Tool launch help (claude/gemini/codex/opencode/kilo/pi/omp/antigravity/cursor/kimi/copilot) ─────────────────────

/// Resolve the launch-help spec for a CLI name (`claude`, `agy`, …).
fn get_tool_spec(name: &str) -> Option<&'static crate::integration_spec::IntegrationSpec> {
    let tool: crate::tool::Tool = name.parse().ok()?;
    let spec = tool.spec();
    if !spec.released {
        return None;
    }
    Some(spec)
}

/// Generate tool launch help from the integration spec.
fn generate_tool_help(spec: &crate::integration_spec::IntegrationSpec) -> String {
    // The displayed launch keyword can differ from the canonical internal
    // `spec.name`: Antigravity launches as "agy", Cursor as "cursor-agent"
    // (the real CLI binary — there is no `cursor` command to pass through to).
    let t = match spec.tool {
        crate::tool::Tool::Antigravity => "agy",
        crate::tool::Tool::Cursor => "cursor-agent",
        _ => spec.name,
    };
    let inside_ai = crate::shared::is_inside_ai_tool();
    let term_desc = if inside_ai {
        "Opens new terminal"
    } else {
        "Runs in current terminal"
    };
    let mut lines: Vec<String> = Vec::new();

    // Usage + examples
    lines.push("Usage:".to_string());
    lines.push(aligned(
        &format!("  hcom [N] {t} [args...]"),
        &format!("Launch N {} agents (default N=1)", spec.label),
        38,
    ));
    lines.push(String::new());
    // Example block — all at same indent level using format helper
    let ex = |usage: &str, desc: &str| -> String { aligned(&format!("    {usage}"), desc, 38) };
    lines.push(ex(&format!("hcom {}", t), term_desc));
    lines.push(ex(&format!("hcom 3 {}", t), "Opens 3 new terminal windows"));
    for (u, d) in spec.help.unique_examples {
        lines.push(ex(u, d));
    }

    // hcom flags — shared with resume/fork, plus --device which only applies
    // at launch (resume uses the `<target>:<device>` suffix instead).
    lines.push(String::new());
    lines.push("hcom Flags:".to_string());
    for (flag, desc) in SHARED_LAUNCH_FLAGS {
        let extra = if *flag == "--headless" && spec.tool == crate::tool::Tool::Claude {
            " (use -p instead for print mode)"
        } else {
            ""
        };
        lines.push(format!("    {:<29}{}{}", flag, desc, extra));
    }
    lines.push(format!(
        "    {:<29}{}",
        "--device <name>", "Launch on a remote relay device"
    ));

    // Environment
    lines.push(String::new());
    lines.push("Environment:".to_string());
    if let Some(args_env) = spec.launch.args_env {
        lines.push(format!(
            "    {:<28} Default args (merged with CLI)",
            args_env
        ));
    }
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
    for (u, d) in spec.help.extra_env {
        lines.push(format!("    {:<28} {}", u.trim(), d));
    }

    // Resume / Fork
    lines.push(String::new());
    let has_resume = spec.resume.is_some();
    let has_fork = spec
        .resume
        .as_ref()
        .map(|r| r.fork.is_some())
        .unwrap_or(false);
    if has_fork {
        lines.push("Resume / Fork:".to_string());
        lines.push(
            "    hcom r <target>                Resume by name / session UUID / thread name"
                .to_string(),
        );
        lines.push(
            "    hcom f <target>                Fork an active or stopped session".to_string(),
        );
        lines.push(
            "    (append :<device> to run on a remote device; see `hcom r --help`)".to_string(),
        );
    } else if has_resume {
        lines.push("Resume:".to_string());
        lines.push(
            "    hcom r <target>                Resume by name / session UUID / thread name"
                .to_string(),
        );
        lines.push(format!(
            "  {} does not support session forking (hcom f).",
            spec.label
        ));
    } else {
        lines.push("Resume / Fork:".to_string());
        lines.push(format!(
            "  {} resume/fork is not currently wired through hcom.",
            spec.label
        ));
    }

    // Footer
    lines.push(String::new());
    lines.push("Exit codes:".to_string());
    lines.push("    0  Ready (or process spawned with no inline readiness wait)".to_string());
    lines.push("    1  Spawn error, or one or more instances reported launch_failed".to_string());
    lines.push(
        "    2  Still launching after readiness wait, or blocked on user attention".to_string(),
    );
    lines.push(String::new());
    lines.push(format!("  Run \"{} --help\" for {} options.", t, t));
    lines.push("  Run \"hcom config terminal --info\" for terminal presets.".to_string());

    lines.join("\n")
}

// ── Format a single help entry ──────────────────────────────────────────

/// `left` padded to `col`, then `desc`; a `left` too wide for the column
/// puts `desc` on its own line at `col` instead of running into it.
fn aligned(left: &str, desc: &str, col: usize) -> String {
    let width = left.chars().count();
    if desc.is_empty() {
        left.trim_end().to_string()
    } else if width < col {
        format!("{left:<col$} {desc}")
    } else {
        format!("{left}\n{:col$} {desc}", "")
    }
}

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
        aligned(&format!("  {usage}"), desc, 34)
    } else if usage.ends_with(':') {
        // Section header
        if desc.is_empty() {
            format!("\n{}", usage)
        } else {
            format!("\n{} {}", usage, desc)
        }
    } else {
        // Command line
        aligned(&format!("  hcom {usage}"), desc, 33)
    }
}

/// Format entries from a static slice.
fn format_entries(entries: &[HelpEntry]) -> Vec<String> {
    entries.iter().map(|(u, d)| format_entry(u, d)).collect()
}

// ── Lookup and render ───────────────────────────────────────────────────

/// Ordered list of all commands (for docs generation). Launch tools at the
/// tail must include every released spec name plus public aliases (e.g. `agy`).
/// The `command_names_covers_released_tools` test guards against drift.
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
    "kilo",
    "kilocode",
    "pi",
    "pi-agent",
    "omp",
    "omp-agent",
    "antigravity",
    "agy",
    "cursor",
    "cursor-agent",
    "kimi",
    "copilot",
    "grok",
    "grok-build",
];

fn forkable_tool_names() -> String {
    crate::integration_spec::ALL
        .iter()
        .filter(|spec| {
            spec.released
                && spec
                    .resume
                    .as_ref()
                    .is_some_and(|resume| resume.fork.is_some())
        })
        .map(|spec| spec.name)
        .collect::<Vec<_>>()
        .join("/")
}

/// Get the top-level help text as a String.
pub fn get_help_text() -> String {
    let forkable = forkable_tool_names();
    let launchable = crate::integration_spec::released_tool_names().join("|");
    format!(
        "hcom (hook-comms) v{} - multi-agent communication\n\
\n\
Usage:\n\
\x20 hcom                                  TUI dashboard\n\
\x20 hcom <command>                        Run command\n\
\n\
Launch:\n\
\x20 hcom [N] {launchable} [flags] [tool-args]\n\
\x20 hcom r <target>                       Resume stopped agent\n\
\x20 hcom f <target>                       Fork agent session ({forkable})\n\
\x20 hcom kill <name(s)|tag:T|all>         Kill + close terminal pane\n\
\n\
Commands:\n\
\x20 send         Send message to your buddies\n\
\x20 listen       Block until message or event arrives\n\
\x20 list         Show agents, status, unread counts\n\
\x20 events       Query event stream, manage subscriptions\n\
\x20 bundle       Structured context packages for handoffs\n\
\x20 transcript   Read another agent's conversation\n\
\x20 start        Connect to hcom (run inside any AI tool)\n\
\x20 stop         Disconnect from hcom\n\
\x20 config       Get/set global and per-agent settings\n\
\x20 run          Execute workflow scripts\n\
\x20 relay        Cross-device sync + relay daemon\n\
\x20 archive      Query past hcom sessions\n\
\x20 reset        Archive and clear database\n\
\x20 hooks        Add or remove hooks\n\
\x20 status       Installation and diagnostics\n\
\x20 term         View/inject into agent PTY screens\n\
\x20 update       Check and apply updates",
        env!("CARGO_PKG_VERSION"),
    )
}

/// Flags accepted by both `hcom <tool>` (fresh launch) and `hcom r` / `hcom f`
/// (resume/fork). Indented to 4 spaces for tool help, re-indented for resume.
///
/// NOTE: `--run-here` / `--no-run-here` are intentionally omitted from help.
/// They still work — they're parsed in launch.rs and resume.rs — but they're
/// an internal detail (TUI injects `--no-run-here`) and a power-user escape
/// hatch (unsupported terminal emulators), not something to advertise.
const SHARED_LAUNCH_FLAGS: &[(&str, &str)] = &[
    ("--tag <name>", "Group tag (names become tag-*)"),
    ("--terminal <preset>", "Where new windows open"),
    ("--dir <path>", "Working directory"),
    ("--headless", "Run in background"),
    ("--hcom-prompt <text>", "Initial prompt"),
    ("--hcom-system-prompt <text>", "System prompt"),
];

/// Shared help body for `hcom r` / `hcom f` (both accept the same target
/// forms and launch flags; only the header, blurb, and see-also differ).
fn resume_fork_help(usage_line: &str, blurb: &str, see_also_line: &str) -> String {
    let mut flags = String::new();
    for (flag, desc) in SHARED_LAUNCH_FLAGS {
        flags.push_str(&format!("  {:<34}{}\n", flag, desc));
    }
    flags.push_str(&format!(
        "  {:<34}{}",
        "--go", "Skip preview, run immediately"
    ));
    format!(
        "Usage:\n\
         \x20 {usage_line}\n\
         \n\
         <target> can be:\n\
         \x20 <name>                            hcom name (4-letter)\n\
         \x20 <uuid>                            claude/codex/gemini session UUID\n\
         \x20 ses_<id>                          opencode/kilo session ID\n\
         \x20 <thread-name>                     claude /rename title or codex thread_name\n\
         \x20 <target>:<device>                 run on a remote device via relay\n\
         \n\
         {blurb}\n\
         \n\
         Flags (parsed before tool args; pass `--` to stop parsing):\n\
         {flags}\n\
         \n\
         Extra args after flags are forwarded to the underlying tool.\n\
         \n\
         See also:\n\
         \x20 {see_also_line}",
    )
}

/// Get formatted help for a single command.
pub fn get_command_help(name: &str) -> String {
    collapse_blank_lines(&command_help_raw(name))
}

/// Section headers open with a blank line and tables also carry `("", "")`
/// spacers; collapse runs so a page never shows two blank lines in a row.
fn collapse_blank_lines(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() && out.last().is_none_or(|l| l.trim().is_empty()) {
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out.join("\n")
}

fn format_page(sections: &[&[HelpEntry]]) -> Vec<String> {
    sections.iter().flat_map(|s| format_entries(s)).collect()
}

fn command_help_raw(name: &str) -> String {
    let mut lines = vec!["Usage:".to_string()];

    // Tool launch commands use the template generator
    if let Some(spec) = get_tool_spec(name) {
        return generate_tool_help(spec);
    }

    // Resume / Fork shortcuts — share the target/flag body, differ only on header + see-also.
    if name == "r" || name == "resume" {
        let see_also = format!(
            "hcom f <target>                   Fork an agent session ({})",
            forkable_tool_names()
        );
        return resume_fork_help(
            "hcom r <target> [tool-args...]    Resume a stopped agent",
            "Adopting by UUID or thread-name reclaims the original hcom\n\
             identity if one existed; otherwise a new identity is assigned.\n\
             CWD is recovered from the session's transcript/DB.",
            &see_also,
        );
    }
    if name == "f" || name == "fork" {
        let blurb = format!(
            "Creates a new agent that continues from the forked session.\n\
             Supported tools: {}.\n\
             Remote fork (`:<device>`) requires --dir to pin the target cwd.",
            forkable_tool_names().replace('/', ", ")
        );
        let see_also = "hcom r <target>                   Resume a stopped agent";
        return resume_fork_help(
            "hcom f <target> [tool-args...]    Fork an agent session (active or stopped)",
            &blurb,
            see_also,
        );
    }

    let page: Option<&[&[HelpEntry]]> = match name {
        "list" => Some(&[LIST_HELP]),
        "send" => Some(&[SEND_HELP]),
        "events" => Some(EVENTS_PAGE),
        "bundle" => Some(BUNDLE_PAGE),
        "stop" => Some(&[STOP_HELP]),
        "start" => Some(&[START_HELP]),
        "kill" => Some(&[KILL_HELP]),
        "listen" => Some(&[LISTEN_HELP]),
        "reset" => Some(&[RESET_HELP]),
        "relay" => Some(RELAY_PAGE),
        "archive" => Some(&[ARCHIVE_HELP]),
        "run" => Some(&[RUN_HELP]),
        "status" => Some(&[STATUS_HELP]),
        "update" => Some(&[UPDATE_HELP]),
        "hooks" => Some(HOOKS_PAGE),
        "term" => Some(TERM_PAGE),
        _ => SUBCOMMAND_HELP
            .iter()
            .find(|(topic, _)| *topic == name)
            .map(|(_, page)| *page),
    };
    if let Some(page) = page {
        // A page that opens with a section header needs no "Usage:" above it.
        if page[0][0].0.ends_with(':') {
            lines.clear();
        }
        lines.extend(format_page(page));
        return expand_hook_tools(&lines.join("\n"));
    }

    // Transcript agent filters come from the same canonical backend registry
    // used by parser selection and disk discovery.
    if name == "transcript" {
        lines.extend(format_entries(TRANSCRIPT_HELP));
        let agents = crate::transcript::transcript_tool_names().join(" | ");
        return lines.join("\n").replace("{transcript_agents}", &agents);
    }

    // Config is special: has dynamic config files hint
    if name == "config" {
        lines.extend(format_entries(CONFIG_HELP));
        lines.extend(format_entries(CONFIG_KEYS_HELP));
        lines.push(String::new());
        lines.push(format_entry("", "hcom config <key> --info for details"));
        lines.push(format_entry(
            "",
            "Precedence: defaults < config.toml < env vars",
        ));
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

    // Try parent command (e.g. "bundle foo" -> "bundle")
    if let Some(pos) = name.rfind(' ') {
        return command_help_raw(&name[..pos]);
    }
    format!("Usage: hcom {}", name)
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
            "agy",
            "antigravity",
            "kimi",
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

    #[test]
    fn gemini_help_states_no_fork_support() {
        let help = get_command_help("gemini");
        assert!(help.contains("Gemini does not support session forking (hcom f)."));
        assert!(!help.contains("Resume / Fork:"));
    }

    #[test]
    fn agy_help_uses_full_launch_template_without_fake_args_env() {
        let help = get_command_help("agy");
        assert!(help.contains("Launch N Antigravity agents"));
        assert!(help.contains("hcom antigravity"));
        assert!(help.contains("hcom agy --sandbox"));
        assert!(!help.contains("hcom agy --model"));
        assert!(help.contains("Run \"agy --help\" for agy options."));
        // Resume now supported via --conversation; fork still unsupported.
        assert!(help.contains("hcom r <target>"));
        assert!(help.contains("Antigravity does not support session forking (hcom f)."));
        assert!(!help.contains("HCOM_AGY_ARGS"));
        assert!(!help.contains("HCOM_ANTIGRAVITY_ARGS"));

        let alias_help = get_command_help("antigravity");
        assert_eq!(alias_help, help);
    }

    #[test]
    fn capability_driven_help_lists_current_integrations() {
        let transcript_help = get_command_help("transcript");
        for tool in crate::transcript::transcript_tool_names() {
            assert!(
                transcript_help.contains(tool),
                "transcript help omitted {tool}"
            );
        }

        let hooks_help = get_command_help("hooks");
        for tool in crate::commands::hooks::hook_tools() {
            assert!(
                hooks_help.contains(tool.as_str()),
                "hooks help omitted {tool}"
            );
        }

        // --headless works for every tool (PTY headless), so resume/fork help
        // must not advertise a per-tool restriction.
        for cmd in ["r", "f"] {
            let help = get_command_help(cmd);
            let headless_line = help
                .lines()
                .find(|l| l.trim_start().starts_with("--headless"))
                .unwrap_or_else(|| panic!("{cmd} help missing --headless"));
            assert!(
                !headless_line.contains("only"),
                "{cmd} help restricts --headless: {headless_line}"
            );
        }
        assert!(!get_command_help("f").contains("Kimi"));
    }

    #[test]
    fn overlong_usage_wraps_description_to_its_column() {
        assert_eq!(
            aligned("  hcom kill all", "Kill", 20),
            "  hcom kill all      Kill"
        );
        let wrapped = aligned("  hcom events sub \"SQL WHERE\" [--once]", "Raw SQL", 20);
        let (first, second) = wrapped.split_once('\n').expect("wrapped");
        assert_eq!(first, "  hcom events sub \"SQL WHERE\" [--once]");
        assert_eq!(second.find("Raw SQL"), Some(21));
        assert_eq!(aligned("  --json ", "", 20), "  --json");
    }

    #[test]
    fn help_topic_resolves_subcommands_and_aliases() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(help_topic("events", &args(&["sub"])), "events sub");
        assert_eq!(help_topic("events", &args(&["--agent", "sub"])), "events");
        assert_eq!(help_topic("bundle", &args(&["preview"])), "bundle prepare");
        assert_eq!(help_topic("hooks", &args(&["install"])), "hooks add");
        assert_eq!(help_topic("hooks", &args(&["uninstall"])), "hooks remove");
        assert_eq!(help_topic("list", &args(&["self"])), "list");
    }

    #[test]
    fn nested_help_pages_are_specific() {
        let events_sub = get_command_help("events sub");
        assert!(events_sub.contains("--on-hit <text>"));
        assert!(events_sub.contains("--as <name>"));
        assert!(!events_sub.contains("Wait for a launch batch:"));

        let events_launch = get_command_help("events launch");
        assert!(events_launch.contains("--timeout SEC"));
        assert!(!events_launch.contains("--on-hit"));

        let bundle_create = get_command_help("bundle create");
        assert!(bundle_create.contains("--bundle-file FILE"));
        assert!(!bundle_create.contains("bundle prepare"));

        let relay_daemon = get_command_help("relay daemon");
        assert!(relay_daemon.contains("relay daemon restart"));
        assert!(!relay_daemon.contains("relay connect <token>"));
    }

    #[test]
    fn subcommand_sections_appear_on_parent_page() {
        let sub_only: &[&[HelpEntry]] = &[EVENTS_SQL_POINTER];
        for (topic, sections) in SUBCOMMAND_HELP {
            let parent = topic.split(' ').next().unwrap();
            let parent_page: &[&[HelpEntry]] = match parent {
                "events" => EVENTS_PAGE,
                "bundle" => BUNDLE_PAGE,
                "relay" => RELAY_PAGE,
                "hooks" => HOOKS_PAGE,
                "term" => TERM_PAGE,
                other => panic!("no parent page for {other}"),
            };
            for section in *sections {
                let shared = |s: &&[HelpEntry]| std::ptr::eq(*s, *section);
                assert!(
                    parent_page.iter().any(shared) || sub_only.iter().any(shared),
                    "{topic}: section missing from `hcom {parent} --help`"
                );
            }
        }
    }

    #[test]
    fn help_pages_have_no_double_blank_lines() {
        for (topic, _) in SUBCOMMAND_HELP {
            assert!(!get_command_help(topic).contains("\n\n\n"), "{topic}");
        }
        for name in [
            "events", "bundle", "relay", "hooks", "term", "config", "start",
        ] {
            assert!(!get_command_help(name).contains("\n\n\n"), "{name}");
        }
    }

    #[test]
    fn config_help_covers_registry_keys() {
        let help = get_command_help("config");
        for (key, _, _) in crate::commands::config::CONFIG_KEYS {
            let key = key
                .strip_prefix("HCOM_")
                .unwrap_or(key)
                .to_ascii_lowercase();
            let shown = help.contains(&key) || key.ends_with("_args");
            assert!(
                shown || CONFIG_KEYS_NOT_IN_HELP.contains(&key.as_str()),
                "config key {key} is neither in help nor CONFIG_KEYS_NOT_IN_HELP"
            );
        }
        assert!(!help.contains("relay_token") && !help.contains("dev_root"));
    }

    #[test]
    fn top_level_help_scopes_fork_to_supported_tools() {
        let help = get_help_text();
        assert!(help.contains(
            "hcom f <target>                       Fork agent session (claude/codex/opencode/kilo/pi/omp/grok)"
        ));
        assert!(!help.contains("Fork agent session (claude/codex/opencode/kilo/pi/omp/kimi)"));
        assert_eq!(
            forkable_tool_names(),
            "claude/codex/opencode/kilo/pi/omp/grok"
        );
    }
}
