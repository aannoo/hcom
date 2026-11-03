//! Unified per-tool integration registry.
//!
//! One `IntegrationSpec` constant per [`Tool`](crate::tool::Tool) variant holds
//! the per-tool facts previously scattered across `tool.rs`, `delivery.rs`,
//! `commands/help.rs`, `hooks/family.rs`, `tui/render/agents.rs`, `launcher.rs`,
//! `commands/launch.rs`, and `commands/resume.rs`.
//!
//! The spec is the configuration plane, not "one file to add a tool" (although that would clearly be better if possible):
//! behavioral integration (hook handler bodies, binding, delivery injection,
//! preprocessing, plugins, arg parsers) still lives in dedicated modules.
//!
//! Not included on purpose:
//! - `HOOK_REGISTRY` (Claude-only; lives in `hooks/utils.rs` as its own
//!   registry — already a single source of truth, no drift across tools).
//! - Transcript parser implementations (owned by `transcript::TranscriptBackend`; canonical identity stays `Tool`).
//! - System-prompt env var keys (typed fields on `HcomConfig`; lookup goes
//!   through `config.rs::FIELD_TO_ENV`).

use crate::shared::tool_detection::EnvPredicate;
use crate::tool::Tool;

/// Help entry: `(usage, description)`. Mirrors `commands/help.rs::HelpEntry`.
pub type HelpEntry = (&'static str, &'static str);

/// How hcom recognises the tool from inside its processes. Precedence between
/// tools lives in [`NATIVE_PRECEDENCE`](crate::shared::tool_detection::NATIVE_PRECEDENCE); every tool also
/// matches `HCOM_TOOL=<name>` as a fallback.
#[derive(Debug, Clone, Copy)]
pub struct DetectionSpec {
    /// Env set by the tool itself; any one identifies it.
    pub markers: &'static [EnvPredicate],
    /// Further tool env cleared for hcom-launched children (markers always are).
    pub clear_for_child: &'static [&'static str],
}

impl DetectionSpec {
    pub const NONE: Self = Self {
        markers: &[],
        clear_for_child: &[],
    };
}

/// How the external tool invokes hcom hook commands.
///
/// This is descriptive metadata for compatibility/spec auditing. Runtime hook
/// dispatch is driven by hook command names and per-tool handlers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookInvocation {
    /// JSON payload on stdin (Claude, Gemini, Codex, Antigravity).
    JsonStdin,
    /// argv-style subcommand (OpenCode, Kilo, Pi, OMP).
    Argv,
    /// No hooks (Adhoc).
    None,
}

#[derive(Debug, Clone, Copy)]
pub struct HooksSpec {
    /// Hook command names this tool answers to (e.g. `["poll", "post", …]`).
    pub names: &'static [&'static str],
    /// If set, this tool borrows another tool's hook command names. Routing
    /// should resolve those names to the owner, not the borrowing tool.
    ///
    /// Antigravity borrows Gemini hook names and is identified out-of-band by
    /// `ANTIGRAVITY_AGENT`, so `Tool::from_hook_name("gemini-*")` must resolve
    /// to Gemini even though this spec lists Gemini's hook names.
    pub shared_hooks_with: Option<Tool>,
    pub invocation: HookInvocation,
}

/// PTY delivery gate booleans. Mirrors fields on `delivery::ToolConfig`.
#[derive(Debug, Clone, Copy)]
pub struct GatesSpec {
    pub require_idle: bool,
    pub require_ready_prompt: bool,
    pub require_prompt_empty: bool,
    pub block_on_user_activity: bool,
    pub block_on_approval: bool,
    pub launch_requires_ready: bool,
    /// Treat the plugin's extension bind (a `kind='plugin'` notify endpoint) as
    /// launch readiness, in addition to the on-screen `ready_patterns`. For
    /// plugin-driven tools whose visible chrome is configurable (Pi: quiet or
    /// expanded header; OMP: status-line presets omit the pi glyph), the bind
    /// is the only rendering-independent proof the interactive TUI is up.
    pub launch_ready_on_plugin_bind: bool,
}

/// Tool background-launch capability.
///
/// Captures the spectrum currently expressed by `released_background: bool` and
/// the dispatch table in [`LaunchBackend::resolve`]. Used to:
///
/// - Drive `released_background_tool_names()` (filters for `NativePrint`).
/// - Document, in one place, how each tool behaves when `--headless` is set.
/// - Provide a typed gate for adding future tools: a new spec must pick a
///   variant rather than implicitly defaulting to "no background".
///
/// Variants:
/// - `Unsupported`: tool has no background launch path. Currently only Adhoc.
/// - `HeadlessPty`: background is routed through the PTY wrapper in a detached
///   runner; the live TUI keeps running there. gemini, codex, opencode, agy.
/// - `NativePrint`: background launches a detached process in the tool's own
///   print mode (Claude `-p --output-format stream-json --verbose`), opt-in via
///   an explicit `-p`/`--print` and kept alive across turns by hcom's stop-hook
///   loop. Default Claude `--headless` routes through `HeadlessPty` (live PTY
///   session) instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundMode {
    Unsupported,
    HeadlessPty,
    NativePrint,
}

/// How `--hcom-prompt` becomes tool-side CLI args.
#[derive(Debug, Clone, Copy)]
pub enum InitialPromptShape {
    /// The tool cannot accept an interactive initial prompt at launch.
    Unsupported { reason: &'static str },
    /// `claude` — `--` then positional.
    DashDashPositional,
    /// `gemini`, `codex` — bare positional (interactive).
    Positional,
    /// `opencode --prompt <text>`, `agy --prompt-interactive <text>`.
    Flag(&'static str),
}

#[derive(Debug, Clone, Copy)]
pub struct PtySpec {
    /// Maximum time to wait for the tool-specific ready pattern before
    /// starting delivery anyway. Every known integration declares this
    /// explicitly; ad-hoc commands use the Adhoc profile.
    pub delivery_start_timeout_secs: u64,
    /// How the tool reads its console on Windows; the PTY relay reads the
    /// real console the same way, so keys reach it as they would directly.
    pub console_input: ConsoleInput,
    /// Pause between typing a message and pressing Enter, for TUIs that drop
    /// an Enter arriving while they still process the paste. 0 for none.
    pub pre_enter_settle_ms: u64,
}

/// How a Windows TUI reads keyboard input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleInput {
    /// VT text (`ENABLE_VIRTUAL_TERMINAL_INPUT`): Node/Bun TUIs. Modifiers VT
    /// can't express are lost even when run directly, but terminal replies and
    /// bracketed-paste markers arrive intact.
    Vt,
    /// Input records (`ReadConsoleInputW`): crossterm. Keys keep their
    /// virtual-key code and modifiers, so Shift+Enter differs from Enter.
    Records,
}

#[derive(Debug, Clone, Copy)]
pub struct LaunchSpec {
    /// Env var holding default CLI args (e.g. `HCOM_CLAUDE_ARGS`).
    pub args_env: Option<&'static str>,
    /// Env var holding the tool's config directory (e.g. `CLAUDE_CONFIG_DIR`).
    pub config_dir_env: Option<&'static str>,
    pub initial_prompt: InitialPromptShape,
    /// PTY-by-default. `false` only for `claude` (which has a `claude-pty`
    /// alias for the PTY path).
    pub uses_pty_default: bool,
    /// Max agents per `hcom [N] <tool>` invocation. Claude gets a larger budget
    /// because of background bulk-launch (`-p` mode); others are capped at 10.
    pub max_launch_count: usize,
    /// Background-launch capability (see [`BackgroundMode`]).
    pub background: BackgroundMode,
}

/// Per-tool resume argument shape.
#[derive(Debug, Clone, Copy)]
pub enum ResumeArgs {
    /// `--resume <id>`, `--session <id>`, `--conversation <id>`.
    Flag(&'static str),
    /// `resume <id>` (Codex subcommand).
    Subcommand(&'static str),
}

/// Per-tool fork argument shape.
#[derive(Debug, Clone, Copy)]
pub enum ForkArgs {
    /// Resume + append flag (claude `--fork-session`, opencode `--fork`).
    AppendFlag(&'static str),
    /// Fork is a sibling subcommand (Codex `fork <id>`).
    Subcommand(&'static str),
}

#[derive(Debug, Clone, Copy)]
pub struct ResumeSpec {
    pub resume: ResumeArgs,
    pub fork: Option<ForkArgs>,
    /// Where the resolved settings and session selector belong in the command.
    pub merge: ResumeMerge,
}

/// Render the session selector and already-resolved arguments for `hcom r`/`hcom f`.
#[derive(Debug, Clone, Copy)]
pub enum ResumeMerge {
    /// Resolved settings, then the session selector (before any `--` tail).
    Append,
    /// Session selector, then resolved settings.
    Prepend,
}

/// Known option shapes for argument composition and saved-argument replay.
///
/// Known non-repeatable options use the last occurrence across configured,
/// saved, and explicit arguments. Unknown options and repeatable options stay
/// in order for the tool to interpret. `drop` and `positional_prompt` apply only
/// when inheriting an earlier launch, before explicit overrides are composed.
#[derive(Debug)]
pub struct ArgFlags {
    /// Flags that take one value: the next token, unless it is `--` or a flag
    /// in these tables (so `"- be terse"` is still a value).
    pub value: &'static [&'static str],
    /// Flags with an optional value or several values: they take the following
    /// tokens up to the next flag, even after `--flag=value`.
    pub multi_value: &'static [&'static str],
    /// Root subcommands that terminate optional or variadic option values.
    /// Required single values can still equal a subcommand name.
    pub subcommands: &'static [&'static str],
    /// Flags that take no value.
    pub boolean: &'static [&'static str],
    /// Saved flags dropped with their values: session selectors, prompts, and
    /// cwd/worktree flags (hcom relaunches in the session's directory).
    pub drop: &'static [&'static str],
    /// Flags that accumulate, so a resume copy adds to the saved ones.
    pub repeatable: &'static [&'static str],
    /// `(short, long)` spellings of one flag. Tables above use the long form.
    pub aliases: &'static [(&'static str, &'static str)],
    /// Bare positionals, and everything after `--`, are the launch prompt.
    pub positional_prompt: bool,
    /// Go `flag` parsing: `-name` and `--name` are the same flag.
    pub single_dash_long: bool,
}

impl ArgFlags {
    const EMPTY: Self = Self {
        value: &[],
        multi_value: &[],
        subcommands: &[],
        boolean: &[],
        drop: &[],
        repeatable: &[],
        aliases: &[],
        positional_prompt: false,
        single_dash_long: false,
    };
}

#[derive(Debug, Clone, Copy)]
pub struct HelpSpec {
    pub unique_examples: &'static [HelpEntry],
    pub extra_env: &'static [HelpEntry],
    /// Extra section printed after Environment.
    pub notes: &'static [&'static str],
}

/// Tool-input field mappings used by `extract_tool_detail` for status display.
/// Each category lists the per-tool tool-call names that produce that kind of
/// activity (bash command, file write, subagent delegate).
#[derive(Debug, Clone, Copy, Default)]
pub struct StatusDetailSpec {
    pub bash: &'static [&'static str],
    pub file: &'static [&'static str],
    pub delegate: &'static [&'static str],
}

/// Complete per-tool integration data.
pub struct IntegrationSpec {
    pub tool: Tool,
    pub name: &'static str,
    pub label: &'static str,
    /// CLI aliases beyond `name` (e.g. `["agy"]` for Antigravity).
    pub aliases: &'static [&'static str],
    /// Executable name on PATH. Usually matches `name`; differs for Antigravity
    /// (`agy`).
    pub cli_binary: &'static str,
    /// 4-char TUI prefix shown in multi-tool agent lists.
    pub tui_prefix: &'static str,
    /// Static icon used when agent status doesn't drive the glyph (Adhoc only).
    pub adhoc_icon: Option<&'static str>,
    /// True if this tool is in the public `RELEASED_TOOLS` set.
    pub released: bool,
    /// On-screen markers of an idle, input-ready TUI; any one visible means
    /// ready. Empty disables pattern gating (Adhoc, plugin-bound tools).
    pub ready_patterns: &'static [&'static str],
    pub pty: PtySpec,
    /// Environment variables specific to this tool's instance state that
    /// will corrupt a same-tool child if leaked (session IDs, sandbox modes,
    /// config-directory pointers, process-role assignments, per-instance
    /// server URLs). Stripped before inheritance and `unset` in runner
    /// scripts. Must NOT include auth/API-key env vars or user config
    /// toggles (those forward). These are in ADDITION to TOOL_MARKER_VARS
    /// which already covers the broad-detection markers.
    ///
    /// Entries must meet ALL THREE criteria:
    /// (a) read as input on fresh start
    /// (b) instance-specific (session/thread/sandbox/path/role)
    /// (c) NOT re-set by the child's own launch
    ///
    /// This is a known-vars strip — NOT a completeness guarantee. Unknown
    /// instance-state vars on closed-source tools, or future vars on any
    /// tool, are a documented gap (same risk the PTY path already lives with
    /// today). Recoverable by adding the var when discovered.
    pub instance_state_env: &'static [&'static str],

    pub detection: DetectionSpec,
    pub hooks: HooksSpec,
    pub gates: GatesSpec,
    pub launch: LaunchSpec,
    /// Known argument shapes shared by launch composition and saved-argument replay.
    pub args: &'static ArgFlags,
    pub resume: Option<ResumeSpec>,
    pub help: HelpSpec,
    pub status_detail: StatusDetailSpec,
}

// ── Hook command name tables ────────────────────────────────────────────

const CLAUDE_HOOKS: &[&str] = &[
    "claude-mod-poll",
    "poll",
    "notify",
    "permission-request",
    "pre",
    "post",
    "post-failure",
    "permission-denied",
    "stop-failure",
    "sessionstart",
    "userpromptsubmit",
    "sessionend",
    "subagent-start",
    "subagent-stop",
];

const GEMINI_HOOKS: &[&str] = &[
    "gemini-sessionstart",
    "gemini-beforeagent",
    "gemini-afteragent",
    "gemini-beforetool",
    "gemini-aftertool",
    "gemini-notification",
    "gemini-sessionend",
];

const CODEX_HOOKS: &[&str] = &[
    "codex-sessionstart",
    "codex-userpromptsubmit",
    "codex-pretooluse",
    // Unregistered; still routed so sessions launched before its removal
    // get a no-op instead of a hook error.
    "codex-permissionrequest",
    "codex-posttooluse",
    "codex-stop",
    "codex-interrupt",
];

const OPENCODE_HOOKS: &[&str] = &[
    "opencode-start",
    "opencode-standby",
    "opencode-status",
    "opencode-read",
    "opencode-stop",
    "opencode-approve",
];

const PI_HOOKS: &[&str] = &[
    "pi-start",
    "pi-status",
    "pi-read",
    "pi-beforetool",
    "pi-stop",
];

const HERMES_HOOKS: &[&str] = &[
    "hermes-start",
    "hermes-status",
    "hermes-read",
    "hermes-beforetool",
    "hermes-stop",
    "hermes-bridge",
];

const OMP_HOOKS: &[&str] = &[
    "omp-start",
    "omp-status",
    "omp-read",
    "omp-beforetool",
    "omp-stop",
];

const CURSOR_HOOKS: &[&str] = &[
    "cursor-sessionstart",
    "cursor-beforesubmitprompt",
    "cursor-pretooluse",
    "cursor-posttooluse",
    "cursor-stop",
    "cursor-sessionend",
];

const KIMI_HOOKS: &[&str] = &[
    "kimi-sessionstart",
    "kimi-userpromptsubmit",
    "kimi-pretooluse",
    "kimi-posttooluse",
    "kimi-permissionrequest",
    "kimi-permissionresult",
    "kimi-stop",
    "kimi-sessionend",
    "kimi-subagentstart",
    "kimi-subagentstop",
    "kimi-notification",
];

const QODER_HOOKS: &[&str] = &[
    "qoder-sessionstart",
    "qoder-userpromptsubmit",
    "qoder-pretooluse",
    "qoder-posttooluse",
    "qoder-posttoolusefailure",
    "qoder-permissionrequest",
    "qoder-notification",
    "qoder-stop",
    "qoder-sessionend",
];

const COPILOT_HOOKS: &[&str] = &[
    "copilot-sessionstart",
    "copilot-userpromptsubmit",
    "copilot-pretooluse",
    "copilot-permissionrequest",
    "copilot-posttooluse",
    "copilot-erroroccurred",
    "copilot-posttoolusefailure",
    "copilot-notification",
    "copilot-agentstop",
    "copilot-subagentstart",
    "copilot-subagentstop",
    "copilot-sessionend",
    // Argv commands run by hcom's Copilot extension, not Copilot hook events.
    "copilot-ext-poll",
    "copilot-ext-sent",
];

/// Grok has no hooks: status and delivery come over its ACP (`delivery/grok.rs`).
const GROK_HOOKS: &[&str] = &[];

// ── Help examples / extra-env tables ────────────────────────────────────

const CLAUDE_HELP_EXAMPLES: &[HelpEntry] = &[(
    "hcom claude --model fable|opus|sonnet|haiku",
    "Flags forwarded to claude",
)];
const CLAUDE_HELP_EXTRA_ENV: &[HelpEntry] = &[(
    "HCOM_SUBAGENT_TIMEOUT",
    "Seconds subagents keep-alive after task",
)];

const GEMINI_HELP_EXAMPLES: &[HelpEntry] = &[
    ("hcom N gemini --yolo", "Flags forwarded to gemini"),
    (
        "hcom gemini --model pro|flash|flash-lite",
        "Use a specific model",
    ),
];
const GEMINI_HELP_EXTRA_ENV: &[HelpEntry] =
    &[("HCOM_GEMINI_SYSTEM_PROMPT", "System prompt (env var)")];

const CODEX_HELP_EXAMPLES: &[HelpEntry] = &[(
    "hcom codex --sandbox danger-full-access",
    "Flags forwarded to codex",
)];
const CODEX_HELP_EXTRA_ENV: &[HelpEntry] = &[(
    "HCOM_CODEX_SYSTEM_PROMPT",
    "System prompt (env var or config)",
)];

const OPENCODE_HELP_EXAMPLES: &[HelpEntry] =
    &[("hcom opencode --agent plan", "Flags forwarded to opencode")];

const KILO_HELP_EXAMPLES: &[HelpEntry] = &[(
    "hcom kilo --model kilo/kilo-auto/free",
    "Flags forwarded to kilo",
)];

const PI_HELP_EXAMPLES: &[HelpEntry] = &[("hcom pi --thinking high", "Flags forwarded to pi")];

const HERMES_HELP_EXAMPLES: &[HelpEntry] = &[
    ("hcom hermes -p work", "Run in a Hermes profile"),
    ("hcom hermes --tui", "Flags forwarded to hermes chat"),
];

const HERMES_HELP_NOTES: &[&str] = &[
    "Profile: -p NAME, else HERMES_HOME, else Hermes's active profile.",
    "    Needs plugins.isolation: in_process.",
    "",
    "Gateway chats (Telegram, Discord, ...):",
    "    hcom hooks add hermes              Install; a running gateway loads it (~20s)",
    "    hcom hooks status hermes           Check install, list connected chats",
    "    hcom hooks remove hermes           Disconnect the profile's chats, remove plugin",
    "    /hcom                              In a chat: show its hcom name",
    "    /hcom start [name]                 Connect (agents: hcom send @name)",
    "    /hcom stop                         Disconnect (shell: hcom stop <name>)",
    "    /hcom visibility off|preview|full  Show incoming messages in the chat",
    "                                       (default preview: first 240 chars)",
    "    Chats stay connected across gateway restarts.",
];

const OMP_HELP_EXAMPLES: &[HelpEntry] = &[("hcom omp --thinking high", "Flags forwarded to omp")];

const AGY_HELP_EXAMPLES: &[HelpEntry] = &[
    ("hcom antigravity", "Long-form alias"),
    ("hcom agy --sandbox", "Flags forwarded to agy"),
    ("hcom r <name>", "Resume a stopped agy session"),
];

const CURSOR_HELP_EXAMPLES: &[HelpEntry] = &[
    ("hcom cursor-agent --model auto", "Use a specific model"),
    (
        "hcom cursor-agent --force",
        "Allow commands unless explicitly denied",
    ),
    (
        "hcom cursor-agent --plan",
        "Flags forwarded to cursor-agent",
    ),
];

const KIMI_HELP_EXAMPLES: &[HelpEntry] = &[
    (
        "hcom kimi --yolo",
        "Auto-run routine actions; risky ones still ask",
    ),
    ("hcom kimi --auto", "Never ask for approval"),
    ("hcom kimi --plan", "Flags forwarded to kimi"),
];

const GROK_HELP_EXAMPLES: &[HelpEntry] = &[
    (
        "hcom grok --reasoning-effort high",
        "Flags forwarded to grok",
    ),
    ("hcom grok --always-approve", "Auto-approve tool executions"),
];

const COPILOT_HELP_EXAMPLES: &[HelpEntry] = &[
    ("hcom copilot --model auto", "Use a specific model"),
    (
        "hcom copilot --allow-tool 'shell(hcom:*)'",
        "Flags forwarded to copilot",
    ),
];

const QODER_HELP_EXAMPLES: &[HelpEntry] = &[
    ("hcom qoder --model Qwen3.8-Flash", "Use a specific model"),
    (
        "hcom qoder --permission-mode accept_edits",
        "Flags forwarded to qodercli",
    ),
];

// ── Resume flag tables ──────────────────────────────────────────────────
//
// Taken from each CLI's own option parser or `--help`. A value flag missing
// here loses its value on resume, so keep `value` complete.

/// These are composition hints, not validators: upstream flags not listed here
/// still pass through. Config overrides remain ordered/repeatable.
static CODEX_ARG_FLAGS: ArgFlags = ArgFlags {
    value: &[
        "--model",
        "--sandbox",
        "--profile",
        "--ask-for-approval",
        "--cd",
        "--config",
        "--image",
        "--add-dir",
        "--enable",
        "--disable",
        "--local-provider",
        "--remote",
        "--remote-auth-token-env",
    ],
    boolean: &[
        "--oss",
        "--search",
        "--no-alt-screen",
        "--no-daemon",
        "--strict-config",
        "--dangerously-bypass-approvals-and-sandbox",
        "--dangerously-bypass-hook-trust",
        "--approve-for-me",
        "--worktree",
        "--help",
        "--version",
        "--last",
        "--all",
        "--include-non-interactive",
    ],
    repeatable: &["--config", "--image", "--add-dir", "--enable", "--disable"],
    aliases: &[
        ("-m", "--model"),
        ("-s", "--sandbox"),
        ("-p", "--profile"),
        ("-a", "--ask-for-approval"),
        ("-C", "--cd"),
        ("-c", "--config"),
        ("-i", "--image"),
        ("-h", "--help"),
        ("-V", "--version"),
        ("--yolo", "--dangerously-bypass-approvals-and-sandbox"),
        ("--not-so-yolo", "--approve-for-me"),
    ],
    drop: &[
        "--cd",
        "--image",
        "--worktree",
        "--last",
        "--all",
        "--include-non-interactive",
    ],
    positional_prompt: true,
    ..ArgFlags::EMPTY
};

static CLAUDE_ARG_FLAGS: ArgFlags = ArgFlags {
    value: &[
        "--model",
        "--fallback-model",
        "--effort",
        "--permission-mode",
        "--system-prompt",
        "--append-system-prompt",
        "--system-prompt-file",
        "--append-system-prompt-file",
        "--settings",
        "--setting-sources",
        "--agent",
        "--agents",
        "--output-format",
        "--input-format",
        "--json-schema",
        "--name",
        "--session-id",
        "--max-budget-usd",
        "--plugin-dir",
    ],
    multi_value: &[
        "--resume",
        "--worktree",
        "--debug",
        "--add-dir",
        "--allowed-tools",
        "--disallowed-tools",
        "--tools",
        "--mcp-config",
        "--betas",
    ],
    boolean: &[
        "--continue",
        "--fork-session",
        "--print",
        "--verbose",
        "--dangerously-skip-permissions",
        "--allow-dangerously-skip-permissions",
        "--strict-mcp-config",
        "--no-session-persistence",
        "--help",
        "--version",
    ],
    repeatable: &["--add-dir", "--mcp-config", "--plugin-dir", "--betas"],
    aliases: &[
        ("-r", "--resume"),
        ("-c", "--continue"),
        ("-p", "--print"),
        ("-n", "--name"),
        ("-w", "--worktree"),
        ("-d", "--debug"),
        ("-h", "--help"),
        ("-v", "--version"),
        ("--allowedTools", "--allowed-tools"),
        ("--disallowedTools", "--disallowed-tools"),
    ],
    drop: &[
        "--resume",
        "--continue",
        "--fork-session",
        "--session-id",
        "--worktree",
    ],
    positional_prompt: true,
    ..ArgFlags::EMPTY
};

static GEMINI_ARG_FLAGS: ArgFlags = ArgFlags {
    value: &[
        "--model",
        "--prompt",
        "--prompt-interactive",
        "--approval-mode",
        "--output-format",
        "--resume",
        "--session-id",
        "--session-file",
    ],
    multi_value: &[
        "--worktree",
        "--include-directories",
        "--extensions",
        "--allowed-tools",
        "--allowed-mcp-server-names",
        "--policy",
        "--admin-policy",
    ],
    boolean: &["--debug", "--sandbox", "--yolo", "--help", "--version"],
    repeatable: &[
        "--include-directories",
        "--extensions",
        "--allowed-tools",
        "--allowed-mcp-server-names",
        "--policy",
        "--admin-policy",
    ],
    aliases: &[
        ("-m", "--model"),
        ("-p", "--prompt"),
        ("-i", "--prompt-interactive"),
        ("-r", "--resume"),
        ("-e", "--extensions"),
        ("-d", "--debug"),
        ("-w", "--worktree"),
        ("-o", "--output-format"),
        ("-s", "--sandbox"),
        ("-y", "--yolo"),
        ("-h", "--help"),
        ("-v", "--version"),
    ],
    drop: &[
        "--resume",
        "--prompt",
        "--prompt-interactive",
        "--worktree",
        "--session-id",
        "--session-file",
    ],
    positional_prompt: true,
    ..ArgFlags::EMPTY
};

static HERMES_ARG_FLAGS: ArgFlags = ArgFlags {
    value: &[
        "--profile",
        "--model",
        "--provider",
        "--toolsets",
        "--skills",
        "--resume",
        "--in",
        "--reasoning",
        "--query",
        "--query-file",
        "--image",
        "--format",
        "--max-turns",
        "--run-budget",
        "--source",
        "--usage-file",
    ],
    multi_value: &["--continue"],
    subcommands: &["chat"],
    boolean: &["--tui", "--yolo", "--help", "--version"],
    aliases: &[
        ("-p", "--profile"),
        ("-m", "--model"),
        ("-t", "--toolsets"),
        ("-s", "--skills"),
        ("-r", "--resume"),
        ("-q", "--query"),
        ("-c", "--continue"),
    ],
    drop: &[
        "--resume",
        "--continue",
        "--query",
        "--query-file",
        "--image",
    ],
    ..ArgFlags::EMPTY
};

/// OpenCode and Kilo (yargs). The positional is the project dir, not a prompt.
static OPENCODE_RESUME_FLAGS: ArgFlags = ArgFlags {
    value: &[
        "--log-level",
        "--port",
        "--hostname",
        "--mdns-domain",
        "--model",
        "--session",
        "--prompt",
        "--agent",
        "--replay-limit",
        // Kilo only.
        "--worktree",
    ],
    multi_value: &["--cors"],
    drop: &[
        "--session",
        "--continue",
        "--fork",
        "--prompt",
        "--worktree",
        "--cloud-fork",
    ],
    repeatable: &["--cors"],
    aliases: &[("-m", "--model"), ("-s", "--session"), ("-c", "--continue")],
    ..ArgFlags::EMPTY
};

/// Antigravity (Go `flag`).
static ANTIGRAVITY_RESUME_FLAGS: ArgFlags = ArgFlags {
    value: &[
        "--add-dir",
        "--agent",
        "--conversation",
        "--effort",
        "--input-format",
        "--json-schema",
        "--log-file",
        "--mode",
        "--model",
        "--output-format",
        "--print-timeout",
        "--project",
        "--prompt-interactive",
    ],
    // `-p "text"`: drop the prompt whichever way agy parses it.
    multi_value: &["--print", "--prompt"],
    drop: &[
        "--conversation",
        "--continue",
        "--prompt-interactive",
        "--print",
        "--prompt",
        "--new-project",
    ],
    repeatable: &["--add-dir"],
    aliases: &[
        ("-c", "--continue"),
        ("-i", "--prompt-interactive"),
        ("-p", "--print"),
    ],
    single_dash_long: true,
    ..ArgFlags::EMPTY
};

/// Cursor (commander). Print flags stay: the launcher rejects them loudly.
static CURSOR_RESUME_FLAGS: ArgFlags = ArgFlags {
    value: &[
        "--api-key",
        "--header",
        "--endpoint",
        "--output-format",
        "--mode",
        "--model",
        "--sandbox",
        "--workspace",
        "--add-dir",
        "--plugin-dir",
        "--worktree-base",
    ],
    multi_value: &["--resume", "--worktree"],
    drop: &[
        "--resume",
        "--continue",
        "--workspace",
        "--worktree",
        "--worktree-base",
        "--skip-worktree-setup",
    ],
    repeatable: &["--header", "--add-dir", "--plugin-dir"],
    aliases: &[
        ("-H", "--header"),
        ("-e", "--endpoint"),
        ("-p", "--print"),
        ("-f", "--force"),
        ("-w", "--worktree"),
    ],
    positional_prompt: true,
    ..ArgFlags::EMPTY
};

/// Kimi. `--agent`/`--agent-file` cannot be combined with `--session`.
static KIMI_RESUME_FLAGS: ArgFlags = ArgFlags {
    value: &[
        "--model",
        "--prompt",
        "--output-format",
        "--skills-dir",
        "--agent",
        "--agent-file",
        "--add-dir",
    ],
    multi_value: &["--session"],
    drop: &[
        "--session",
        "--continue",
        "--prompt",
        "--agent",
        "--agent-file",
    ],
    repeatable: &["--skills-dir", "--add-dir"],
    aliases: &[
        ("-S", "--session"),
        ("-c", "--continue"),
        ("-m", "--model"),
        ("-p", "--prompt"),
        ("-y", "--yolo"),
    ],
    ..ArgFlags::EMPTY
};

/// Pi. The initial prompt is positional.
static PI_RESUME_FLAGS: ArgFlags = ArgFlags {
    boolean: &[
        "--print",
        "--continue",
        "--resume",
        "--no-session",
        "--no-tools",
        "--no-builtin-tools",
        "--no-extensions",
        "--no-mcp",
        "--no-skills",
        "--no-prompt-templates",
        "--no-themes",
        "--no-context-files",
        "--verbose",
        "--approve",
        "--no-approve",
        "--offline",
        "--help",
        "--version",
    ],
    value: &[
        "--provider",
        "--model",
        "--api-key",
        "--system-prompt",
        "--append-system-prompt",
        "--mode",
        "--session",
        "--session-id",
        "--fork",
        "--session-dir",
        "--name",
        "--models",
        "--tools",
        "--exclude-tools",
        "--thinking",
        "--extension",
        "--skill",
        "--prompt-template",
        "--theme",
        "--use-theme",
        "--export",
        "--tui-mode",
    ],
    multi_value: &["--list-models"],
    drop: &[
        "--session",
        "--session-id",
        "--session-dir",
        "--fork",
        "--continue",
        "--resume",
        "--no-session",
        "--print",
    ],
    repeatable: &[
        "--append-system-prompt",
        "--extension",
        "--skill",
        "--prompt-template",
        "--theme",
    ],
    aliases: &[
        ("-p", "--print"),
        ("-c", "--continue"),
        ("-r", "--resume"),
        ("-n", "--name"),
        ("-t", "--tools"),
        ("-xt", "--exclude-tools"),
        ("-e", "--extension"),
        ("-nt", "--no-tools"),
        ("-nbt", "--no-builtin-tools"),
        ("-ne", "--no-extensions"),
        ("-ns", "--no-skills"),
        ("-np", "--no-prompt-templates"),
        ("-nc", "--no-context-files"),
        ("-a", "--approve"),
        ("-na", "--no-approve"),
    ],
    positional_prompt: true,
    ..ArgFlags::EMPTY
};

/// OMP (`cli/flag-tables.ts`). The initial prompt is positional.
static OMP_RESUME_FLAGS: ArgFlags = ArgFlags {
    boolean: &[
        "--help",
        "--version",
        "--allow-home",
        "--continue",
        "--no-session",
        "--no-tools",
        "--no-lsp",
        "--no-pty",
        "--hide-thinking",
        "--advisor",
        "--external-thinking",
        "--prewalk",
        "--no-prewalk",
        "--plan-yolo",
        "--print",
        "--print-thoughts",
        "--no-extensions",
        "--no-skills",
        "--no-rules",
        "--no-title",
        "--no-ui",
        "--auto-approve",
        "--yolo",
    ],
    value: &[
        "--cwd",
        "--config",
        "--add-dir",
        "--mode",
        "--fork",
        "--provider",
        "--model",
        "--smol",
        "--slow",
        "--goal",
        "--plan",
        "--prewalk-into",
        "--plan-yolo-into",
        "--max-time",
        "--service-tier",
        "--api-key",
        "--system-prompt",
        "--system-prompt-template",
        "--append-system-prompt",
        "--provider-session-id",
        "--prompt-cache-key",
        "--session-dir",
        "--session-id",
        "--models",
        "--tools",
        "--thinking",
        "--export",
        "--hook",
        "--extension",
        "--trusted-extension",
        "--plugin-dir",
        "--skills",
        "--approval-mode",
        "--profile",
        "--alias",
    ],
    multi_value: &["--resume", "--session", "--from-claude", "--from-codex"],
    drop: &[
        "--resume",
        "--session",
        "--session-id",
        "--session-dir",
        "--fork",
        "--continue",
        "--from-claude",
        "--from-codex",
        "--goal",
        "--no-session",
        "--print",
        "--cwd",
    ],
    repeatable: &[
        "--config",
        "--add-dir",
        "--hook",
        "--extension",
        "--trusted-extension",
        "--plugin-dir",
    ],
    aliases: &[
        ("-r", "--resume"),
        ("-c", "--continue"),
        ("-e", "--extension"),
        ("-p", "--print"),
    ],
    positional_prompt: true,
    ..ArgFlags::EMPTY
};

/// Copilot (commander). Saved permission flags are dropped.
static COPILOT_RESUME_FLAGS: ArgFlags = ArgFlags {
    value: &[
        "--interactive",
        "--prompt",
        "--model",
        "--reasoning-effort",
        "--context",
        "--auto-tier",
        "--agent",
        "--name",
        "--session-id",
        "-C",
        "--log-dir",
        "--extension-sdk-path",
        "--log-level",
        "--stream",
        "--output-format",
        "--add-dir",
        "--attachment",
        "--disable-mcp-server",
        "--enable-mcp-server",
        "--add-github-mcp-toolset",
        "--add-github-mcp-tool",
        "--plugin-dir",
        "--additional-mcp-config",
        "--mcp-github-auth",
        "--max-autopilot-continues",
        "--mode",
        "--environment-host",
        "--environment-compute-id",
        "--dynamic-retrieval",
        "--max-ai-credits",
        "--usage-output-file",
    ],
    multi_value: &[
        "--resume",
        "--connect",
        "--share",
        "--allow-tool",
        "--deny-tool",
        "--available-tools",
        "--excluded-tools",
        "--secret-env-vars",
        "--allow-url",
        "--deny-url",
        "--bash-env",
        "--mouse",
    ],
    drop: &[
        "--resume",
        "--continue",
        "--connect",
        "--session-id",
        "--interactive",
        "--prompt",
        "-C",
        "--allow-all-tools",
        "--allow-all",
        "--yolo",
    ],
    repeatable: &[
        "--add-dir",
        "--attachment",
        "--plugin-dir",
        "--disable-mcp-server",
        "--enable-mcp-server",
        "--add-github-mcp-toolset",
        "--add-github-mcp-tool",
        "--allow-tool",
        "--deny-tool",
        "--allow-url",
        "--deny-url",
        "--secret-env-vars",
    ],
    aliases: &[
        ("-i", "--interactive"),
        ("-p", "--prompt"),
        ("-r", "--resume"),
        ("-n", "--name"),
        ("-s", "--silent"),
    ],
    positional_prompt: true,
    ..ArgFlags::EMPTY
};

/// Qoder. `--tools` takes every value up to the next flag.
static QODER_RESUME_FLAGS: ArgFlags = ArgFlags {
    value: &[
        "--model",
        "--reasoning-effort",
        "--thinking",
        "--thinking-budget",
        "--context-window",
        "--config-dir",
        "--permission-mode",
        "--allowed-mcp-server-names",
        "--allowed-tools",
        "--disallowed-tools",
        "--attachment",
        "--plugin-dir",
        "--name",
        "--add-dir",
        "--agent",
        "--agents",
        "--append-system-prompt",
        "--system-prompt",
        "--output-style",
        "--mcp-config",
        "--settings",
        "--setting-sources",
        "--max-model-request-retries",
        "--max-turns",
        "--max-output-tokens",
        "--output-format",
        "--input-format",
        "--session-id",
        "--prompt-interactive",
        "--cwd",
        "--remote-session",
        "--teleport",
        "--remote-control",
    ],
    multi_value: &["--tools", "--resume", "--worktree", "--remote"],
    drop: &[
        "--resume",
        "--session-id",
        "--continue",
        "--fork-session",
        "--prompt-interactive",
        "--cwd",
        "--worktree",
        "--remote",
        "--remote-session",
        "--teleport",
        "--remote-control",
    ],
    repeatable: &["--add-dir"],
    aliases: &[
        ("-m", "--model"),
        ("-n", "--name"),
        ("-o", "--output-format"),
        ("-r", "--resume"),
        ("-c", "--continue"),
        ("-i", "--prompt-interactive"),
        ("-w", "--cwd"),
    ],
    positional_prompt: true,
    ..ArgFlags::EMPTY
};

/// Grok (clap, which rejects a repeated singular flag). `--worktree` takes an optional value.
static GROK_RESUME_FLAGS: ArgFlags = ArgFlags {
    boolean: &["--always-approve"],
    value: &[
        "--model",
        "--rules",
        "--agent",
        "--permission-mode",
        "--reasoning-effort",
        "--effort",
        "--max-turns",
        "--output-format",
        "--disallowed-tools",
        "--tools",
        "--sandbox",
        "--debug-file",
        "--system-prompt-override",
        "--system-prompt",
        "--agents",
        "--json-schema",
        "--cwd",
        "--session-id",
        "--single",
        "--prompt-file",
        "--prompt-json",
        "--worktree-ref",
        "--ref",
        "--allow",
        "--deny",
        "--leader-socket",
    ],
    multi_value: &["--resume", "--worktree"],
    drop: &[
        "--cwd",
        "--resume",
        "--session-id",
        "--single",
        "--prompt-file",
        "--prompt-json",
        "--worktree",
        "--worktree-ref",
        "--ref",
        "--continue",
        "--fork-session",
        "--restore-code",
    ],
    repeatable: &["--allow", "--deny"],
    aliases: &[
        ("-m", "--model"),
        ("-r", "--resume"),
        ("-s", "--session-id"),
        ("-p", "--single"),
        ("-w", "--worktree"),
        ("-c", "--continue"),
    ],
    positional_prompt: true,
    ..ArgFlags::EMPTY
};

// ── Per-tool integration constants ──────────────────────────────────────

pub static CLAUDE: IntegrationSpec = IntegrationSpec {
    tool: Tool::Claude,
    name: "claude",
    label: "Claude",
    aliases: &[],
    cli_binary: "claude",
    tui_prefix: "cla ",
    adhoc_icon: None,
    released: true,
    // Default mode shows "? for shortcuts"; every other permission mode
    // (acceptEdits, plan, auto, dontAsk, bypassPermissions) replaces it with
    // "<mode> on (<binding> to cycle)". The binding is configurable (shift+tab
    // by default, meta+m on Windows without VT), so match only its tail.
    ready_patterns: &["? for shortcuts", "to cycle)"],
    pty: PtySpec {
        delivery_start_timeout_secs: 5,
        console_input: ConsoleInput::Vt,
        pre_enter_settle_ms: 0,
    },
    // Set in Claude's Bash tool for the parent session. CHILD_SESSION turns
    // off transcript saving, which would break resume, fork and lineage.
    instance_state_env: &[
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_MESSAGING_TOKEN",
    ],
    detection: DetectionSpec {
        markers: &[
            EnvPredicate::equals("CLAUDECODE", "1"),
            EnvPredicate::non_empty("CLAUDE_ENV_FILE"),
        ],
        ..DetectionSpec::NONE
    },
    hooks: HooksSpec {
        names: CLAUDE_HOOKS,
        shared_hooks_with: None,
        invocation: HookInvocation::JsonStdin,
    },
    // - require_ready_prompt=false: status bar hides in accept-edits mode.
    // - require_prompt_empty=true: VT100 dim detection separates placeholder
    //   (dim) from user input (not dim).
    gates: GatesSpec {
        require_idle: true,
        require_ready_prompt: false,
        require_prompt_empty: true,
        block_on_user_activity: true,
        block_on_approval: true,
        launch_requires_ready: false,
        launch_ready_on_plugin_bind: false,
    },
    launch: LaunchSpec {
        args_env: Some("HCOM_CLAUDE_ARGS"),
        config_dir_env: Some("CLAUDE_CONFIG_DIR"),
        initial_prompt: InitialPromptShape::DashDashPositional,
        uses_pty_default: false,
        max_launch_count: 100,
        background: BackgroundMode::NativePrint,
    },
    args: &CLAUDE_ARG_FLAGS,
    resume: Some(ResumeSpec {
        resume: ResumeArgs::Flag("--resume"),
        fork: Some(ForkArgs::AppendFlag("--fork-session")),
        merge: ResumeMerge::Append,
    }),
    help: HelpSpec {
        unique_examples: CLAUDE_HELP_EXAMPLES,
        extra_env: CLAUDE_HELP_EXTRA_ENV,
        notes: &[],
    },
    status_detail: StatusDetailSpec {
        bash: &["Bash"],
        file: &["Write", "Edit"],
        delegate: &["Task", "Agent"],
    },
};

pub static GEMINI: IntegrationSpec = IntegrationSpec {
    tool: Tool::Gemini,
    name: "gemini",
    label: "Gemini",
    aliases: &[],
    cli_binary: "gemini",
    tui_prefix: "gem ",
    adhoc_icon: None,
    released: true,
    ready_patterns: &["Type your message"],
    pty: PtySpec {
        delivery_start_timeout_secs: 60,
        console_input: ConsoleInput::Vt,
        pre_enter_settle_ms: 0,
    },
    instance_state_env: &["GEMINI_PTY_INFO"],
    detection: DetectionSpec {
        markers: &[EnvPredicate::equals("GEMINI_CLI", "1")],
        clear_for_child: &["GEMINI_SYSTEM_MD", "GEMINI_CLI_NO_RELAUNCH"],
    },
    hooks: HooksSpec {
        names: GEMINI_HOOKS,
        shared_hooks_with: None,
        invocation: HookInvocation::JsonStdin,
    },
    // require_ready_prompt=true: "Type your message" placeholder disappears
    // instantly when user types, so visibility implies an empty prompt.
    gates: GatesSpec {
        require_idle: true,
        require_ready_prompt: true,
        require_prompt_empty: false,
        block_on_user_activity: true,
        block_on_approval: true,
        launch_requires_ready: true,
        launch_ready_on_plugin_bind: false,
    },
    launch: LaunchSpec {
        args_env: Some("HCOM_GEMINI_ARGS"),
        config_dir_env: Some("GEMINI_CLI_HOME"),
        initial_prompt: InitialPromptShape::Positional,
        uses_pty_default: true,
        max_launch_count: 10,
        background: BackgroundMode::HeadlessPty,
    },
    args: &GEMINI_ARG_FLAGS,
    resume: Some(ResumeSpec {
        resume: ResumeArgs::Flag("--resume"),
        fork: None,
        merge: ResumeMerge::Append,
    }),
    help: HelpSpec {
        unique_examples: GEMINI_HELP_EXAMPLES,
        extra_env: GEMINI_HELP_EXTRA_ENV,
        notes: &[],
    },
    status_detail: StatusDetailSpec {
        bash: &["run_shell_command"],
        file: &["write_file", "replace"],
        delegate: &["invoke_agent"],
    },
};

pub static CODEX: IntegrationSpec = IntegrationSpec {
    tool: Tool::Codex,
    name: "codex",
    label: "Codex",
    aliases: &[],
    cli_binary: "codex",
    tui_prefix: "cod ",
    adhoc_icon: None,
    released: true,
    // "» " is the Ultra composer prompt.
    ready_patterns: &["\u{203A} ", "\u{BB} "],
    pty: PtySpec {
        delivery_start_timeout_secs: 5,
        console_input: ConsoleInput::Records,
        pre_enter_settle_ms: 0,
    },
    instance_state_env: &["CODEX_EXEC_SERVER_URL"],
    detection: DetectionSpec {
        markers: &[
            EnvPredicate::set("CODEX_SANDBOX"),
            EnvPredicate::set("CODEX_SANDBOX_NETWORK_DISABLED"),
            EnvPredicate::set("CODEX_MANAGED_BY_NPM"),
            EnvPredicate::set("CODEX_MANAGED_BY_BUN"),
            EnvPredicate::set("CODEX_THREAD_ID"),
            EnvPredicate::set("CODEX_SESSION_ID"),
        ],
        ..DetectionSpec::NONE
    },
    hooks: HooksSpec {
        names: CODEX_HOOKS,
        shared_hooks_with: None,
        invocation: HookInvocation::JsonStdin,
    },
    // - require_ready_prompt=true: no composer (or a startup prompt / picker
    //   drawn over it) reads as not_ready, not as uncommitted text.
    // - require_prompt_empty=true: VT100 dim detection after `›` or `»`.
    gates: GatesSpec {
        require_idle: true,
        require_ready_prompt: true,
        require_prompt_empty: true,
        block_on_user_activity: true,
        block_on_approval: true,
        launch_requires_ready: false,
        launch_ready_on_plugin_bind: false,
    },
    launch: LaunchSpec {
        args_env: Some("HCOM_CODEX_ARGS"),
        config_dir_env: Some("CODEX_HOME"),
        initial_prompt: InitialPromptShape::Positional,
        uses_pty_default: true,
        max_launch_count: 10,
        background: BackgroundMode::HeadlessPty,
    },
    args: &CODEX_ARG_FLAGS,
    resume: Some(ResumeSpec {
        resume: ResumeArgs::Subcommand("resume"),
        fork: Some(ForkArgs::Subcommand("fork")),
        merge: ResumeMerge::Prepend,
    }),
    help: HelpSpec {
        unique_examples: CODEX_HELP_EXAMPLES,
        extra_env: CODEX_HELP_EXTRA_ENV,
        notes: &[],
    },
    // Codex hooks report every shell/exec tool as `Bash` (HookToolName::bash).
    status_detail: StatusDetailSpec {
        bash: &["Bash"],
        file: &["apply_patch"],
        delegate: &["spawn_agent"],
    },
};

pub static OPENCODE: IntegrationSpec = IntegrationSpec {
    tool: Tool::OpenCode,
    name: "opencode",
    label: "OpenCode",
    aliases: &[],
    cli_binary: "opencode",
    tui_prefix: "opc ",
    adhoc_icon: None,
    released: true,
    ready_patterns: &["ctrl+p commands"],
    pty: PtySpec {
        delivery_start_timeout_secs: 5,
        console_input: ConsoleInput::Vt,
        pre_enter_settle_ms: 0,
    },
    instance_state_env: &["OPENCODE_RUN_ID", "OPENCODE_PROCESS_ROLE"],
    detection: DetectionSpec {
        markers: &[EnvPredicate::equals("OPENCODE", "1")],
        clear_for_child: &["OPENCODE_PID"],
    },
    hooks: HooksSpec {
        names: OPENCODE_HOOKS,
        shared_hooks_with: None,
        invocation: HookInvocation::Argv,
    },
    // Runtime gates off — the TypeScript plugin owns delivery after bootstrap.
    // launch_requires_ready=true so PTY bootstrap inject lands on a usable TUI.
    gates: GatesSpec {
        require_idle: false,
        require_ready_prompt: false,
        require_prompt_empty: false,
        block_on_user_activity: false,
        block_on_approval: false,
        launch_requires_ready: true,
        launch_ready_on_plugin_bind: false,
    },
    launch: LaunchSpec {
        args_env: Some("HCOM_OPENCODE_ARGS"),
        // OPENCODE_CONFIG_DIR is read by the plugin install/verify/remove path
        // in hooks/opencode.rs; surface it for the launch diagnostic dump.
        config_dir_env: Some("OPENCODE_CONFIG_DIR"),
        initial_prompt: InitialPromptShape::Flag("--prompt"),
        uses_pty_default: true,
        max_launch_count: 10,
        background: BackgroundMode::HeadlessPty,
    },
    args: &OPENCODE_RESUME_FLAGS,
    resume: Some(ResumeSpec {
        resume: ResumeArgs::Flag("--session"),
        fork: Some(ForkArgs::AppendFlag("--fork")),
        merge: ResumeMerge::Prepend,
    }),
    help: HelpSpec {
        unique_examples: OPENCODE_HELP_EXAMPLES,
        extra_env: &[],
        notes: &[],
    },
    // Tool IDs from the plugin's execute.before hook: OpenCode 1 names first,
    // then OpenCode 2's (shell/patch/subagent). Kilo shares the plugin.
    status_detail: StatusDetailSpec {
        bash: &["bash", "shell"],
        file: &["edit", "write", "apply_patch", "patch"],
        delegate: &["task", "subagent"],
    },
};

pub static KILO: IntegrationSpec = IntegrationSpec {
    tool: Tool::Kilo,
    name: "kilo",
    label: "Kilo Code",
    aliases: &["kilocode"],
    cli_binary: "kilo",
    tui_prefix: "kil ",
    adhoc_icon: None,
    released: true,
    ready_patterns: &["ctrl+p commands"],
    // Kilo namespaces OpenCode's run/role state under its own vars (see
    // kilocode packages/core/src/util/opencode-process.ts: KILO_RUN_ID /
    // KILO_PROCESS_ROLE are `??=`-assigned, so an inherited value is reused
    // rather than regenerated → a leaked parent value collides a same-tool
    // child, exactly like OpenCode's OPENCODE_RUN_ID/OPENCODE_PROCESS_ROLE).
    // KILO_DB is the on-disk session store; all three are instance-state.
    pty: PtySpec {
        delivery_start_timeout_secs: 5,
        console_input: ConsoleInput::Vt,
        pre_enter_settle_ms: 0,
    },
    instance_state_env: &["KILO_DB", "KILO_RUN_ID", "KILO_PROCESS_ROLE"],
    detection: DetectionSpec {
        markers: &[EnvPredicate::equals("KILO", "1")],
        ..DetectionSpec::NONE
    },
    hooks: HooksSpec {
        names: OPENCODE_HOOKS,
        shared_hooks_with: Some(Tool::OpenCode),
        invocation: HookInvocation::Argv,
    },
    // Kilo is an OpenCode-family variant: its shared TypeScript plugin owns
    // delivery after the first PTY bootstrap turn.
    gates: GatesSpec {
        require_idle: false,
        require_ready_prompt: false,
        require_prompt_empty: false,
        block_on_user_activity: false,
        block_on_approval: false,
        launch_requires_ready: true,
        launch_ready_on_plugin_bind: false,
    },
    launch: LaunchSpec {
        args_env: Some("HCOM_KILO_ARGS"),
        config_dir_env: Some("KILO_CONFIG_DIR"),
        initial_prompt: InitialPromptShape::Flag("--prompt"),
        uses_pty_default: true,
        max_launch_count: 10,
        background: BackgroundMode::HeadlessPty,
    },
    args: &OPENCODE_RESUME_FLAGS,
    resume: Some(ResumeSpec {
        resume: ResumeArgs::Flag("--session"),
        fork: Some(ForkArgs::AppendFlag("--fork")),
        merge: ResumeMerge::Prepend,
    }),
    help: HelpSpec {
        unique_examples: KILO_HELP_EXAMPLES,
        extra_env: &[],
        notes: &[],
    },
    status_detail: StatusDetailSpec {
        bash: &["bash", "shell"],
        file: &["edit", "write", "apply_patch", "patch"],
        delegate: &["task", "subagent"],
    },
};

pub static ANTIGRAVITY: IntegrationSpec = IntegrationSpec {
    tool: Tool::Antigravity,
    name: "antigravity",
    label: "Antigravity",
    aliases: &["agy"],
    cli_binary: "agy",
    tui_prefix: "agy ",
    adhoc_icon: None,
    released: true,
    ready_patterns: &["? for shortcuts"],
    pty: PtySpec {
        delivery_start_timeout_secs: 5,
        console_input: ConsoleInput::Vt,
        pre_enter_settle_ms: 0,
    },
    instance_state_env: &["ANTIGRAVITY_EXECUTABLE_DATA_DIR", "GEMINI_PTY_INFO"],
    detection: DetectionSpec {
        markers: &[EnvPredicate::set("ANTIGRAVITY_AGENT")],
        ..DetectionSpec::NONE
    },
    hooks: HooksSpec {
        names: GEMINI_HOOKS,
        // Antigravity reuses Gemini hook names; routing resolves those names
        // to Gemini. Identity is established via ANTIGRAVITY_AGENT.
        shared_hooks_with: Some(Tool::Gemini),
        invocation: HookInvocation::JsonStdin,
    },
    gates: GatesSpec {
        require_idle: true,
        require_ready_prompt: true,
        require_prompt_empty: true,
        block_on_user_activity: false,
        block_on_approval: true,
        launch_requires_ready: true,
        launch_ready_on_plugin_bind: false,
    },
    launch: LaunchSpec {
        args_env: None,
        // Antigravity shares Gemini's config tree.
        config_dir_env: Some("GEMINI_CLI_HOME"),
        initial_prompt: InitialPromptShape::Flag("--prompt-interactive"),
        uses_pty_default: true,
        max_launch_count: 10,
        background: BackgroundMode::HeadlessPty,
    },
    args: &ANTIGRAVITY_RESUME_FLAGS,
    resume: Some(ResumeSpec {
        resume: ResumeArgs::Flag("--conversation"),
        fork: None,
        merge: ResumeMerge::Prepend,
    }),
    help: HelpSpec {
        // Help lookup uses "agy" externally; the agy unique examples document
        // the long-form alias as a forwarded flag.
        unique_examples: AGY_HELP_EXAMPLES,
        extra_env: &[],
        notes: &[],
    },
    status_detail: StatusDetailSpec {
        bash: &["run_command"],
        file: &[
            "write_to_file",
            "replace_file_content",
            "multi_replace_file_content",
        ],
        delegate: &["invoke_subagent"],
    },
};

pub static CURSOR: IntegrationSpec = IntegrationSpec {
    tool: Tool::Cursor,
    name: "cursor",
    label: "Cursor",
    aliases: &["cursor-agent"],
    cli_binary: "cursor-agent",
    tui_prefix: "cur ",
    adhoc_icon: None,
    released: true,
    // Cursor's input placeholder is styled rather than a stable ASCII footer.
    // Prompt-empty detection is the readiness signal for the MVP.
    ready_patterns: &[],
    // Closed-source; unknown instance-state vars are a documented gap.
    pty: PtySpec {
        delivery_start_timeout_secs: 5,
        console_input: ConsoleInput::Vt,
        pre_enter_settle_ms: 0,
    },
    instance_state_env: &[],
    detection: DetectionSpec {
        markers: &[
            EnvPredicate::set("CURSOR_AGENT"),
            EnvPredicate::set("CURSOR_PROJECT_DIR"),
        ],
        ..DetectionSpec::NONE
    },
    hooks: HooksSpec {
        names: CURSOR_HOOKS,
        shared_hooks_with: None,
        invocation: HookInvocation::JsonStdin,
    },
    gates: GatesSpec {
        require_idle: true,
        require_ready_prompt: false,
        require_prompt_empty: true,
        block_on_user_activity: true,
        block_on_approval: true,
        launch_requires_ready: true,
        launch_ready_on_plugin_bind: false,
    },
    launch: LaunchSpec {
        args_env: Some("HCOM_CURSOR_ARGS"),
        config_dir_env: Some("CURSOR_CONFIG_DIR"),
        initial_prompt: InitialPromptShape::Positional,
        uses_pty_default: true,
        max_launch_count: 10,
        background: BackgroundMode::HeadlessPty,
    },
    args: &CURSOR_RESUME_FLAGS,
    resume: Some(ResumeSpec {
        resume: ResumeArgs::Flag("--resume"),
        // O2: cursor-agent has no native fork/branch primitive (only `--resume`
        // / `--continue` / `create-chat`). Leave `fork: None` like gemini/agy;
        // simulating fork via resume+create-chat is deferred (not needed for
        // parity).
        fork: None,
        merge: ResumeMerge::Prepend,
    }),
    help: HelpSpec {
        unique_examples: CURSOR_HELP_EXAMPLES,
        extra_env: &[],
        notes: &[],
    },
    // Empirically verified against real cursor transcripts: cursor's edit tool
    // is `StrReplace` (not `Edit`, which never appears) and its file/edit tools
    // key the path off `path` (not `file_path` — see extract_tool_detail). Shell
    // also has the `run_terminal_cmd` variant; delegates are `Task`/`Subagent`.
    status_detail: StatusDetailSpec {
        bash: &["Shell", "run_terminal_cmd"],
        file: &["Write", "StrReplace"],
        delegate: &["Task", "Subagent"],
    },
};

pub static KIMI: IntegrationSpec = IntegrationSpec {
    tool: Tool::Kimi,
    name: "kimi",
    label: "Kimi",
    aliases: &[],
    cli_binary: "kimi",
    tui_prefix: "kim ",
    adhoc_icon: None,
    released: true,
    ready_patterns: &["> "],
    pty: PtySpec {
        delivery_start_timeout_secs: 5,
        console_input: ConsoleInput::Vt,
        pre_enter_settle_ms: 0,
    },
    instance_state_env: &[],
    detection: DetectionSpec {
        markers: &[
            EnvPredicate::equals("KIMI_CODE_CLI", "1"),
            EnvPredicate::set("KIMI_SESSION_ID"),
        ],
        ..DetectionSpec::NONE
    },
    hooks: HooksSpec {
        names: KIMI_HOOKS,
        shared_hooks_with: None,
        invocation: HookInvocation::JsonStdin,
    },
    gates: GatesSpec {
        require_idle: true,
        require_ready_prompt: false,
        require_prompt_empty: true,
        block_on_user_activity: true,
        block_on_approval: true,
        launch_requires_ready: false,
        launch_ready_on_plugin_bind: false,
    },
    launch: LaunchSpec {
        args_env: Some("HCOM_KIMI_ARGS"),
        // Kimi's data root (config + sessions + credentials) is overridden via
        // KIMI_CODE_HOME; it does not honor a separate config-dir variable.
        config_dir_env: Some("KIMI_CODE_HOME"),
        initial_prompt: InitialPromptShape::Unsupported {
            reason: "kimi does not support an initial prompt at launch. Launch `hcom kimi` without a prompt, then send the task with `hcom send @<name> -- \"…\"`.",
        },
        uses_pty_default: true,
        max_launch_count: 10,
        background: BackgroundMode::HeadlessPty,
    },
    args: &KIMI_RESUME_FLAGS,
    resume: Some(ResumeSpec {
        // Use --session as it is the documented flag in kimi --help.
        resume: ResumeArgs::Flag("--session"),
        // Kimi has no CLI fork primitive — `/fork` is an interactive slash
        // command only (`kimi --fork` errors "unknown option"). Like
        // gemini/cursor/agy, leave fork unsupported.
        fork: None,
        merge: ResumeMerge::Prepend,
    }),
    help: HelpSpec {
        unique_examples: KIMI_HELP_EXAMPLES,
        // No HCOM_KIMI_SYSTEM_PROMPT: kimi has no system-prompt config field or
        // injection path (see hooks/kimi.rs — it's a future kimi feature), so
        // advertising it here was a ghost.
        extra_env: &[],
        notes: &[],
    },
    // Tool names from kimi-code's built-in tools
    // (docs/reference/tools.md): shell is `Bash`, file writes are `Write`/`Edit`,
    // and the subagent tools are `Agent` and `AgentSwarm`.
    status_detail: StatusDetailSpec {
        bash: &["Bash"],
        file: &["Write", "Edit"],
        delegate: &["Agent", "AgentSwarm"],
    },
};

pub static PI: IntegrationSpec = IntegrationSpec {
    tool: Tool::Pi,
    name: "pi",
    label: "Pi",
    aliases: &["pi-agent"],
    cli_binary: "pi",
    tui_prefix: "pi  ",
    adhoc_icon: None,
    released: true,
    // Readiness comes from the hcom extension's bind, not on-screen text.
    // Pi's header hint differs between the compact ("/ commands"), expanded
    // ("/ for commands") and quiet-startup (none) headers, and it is drawn
    // before Pi enables its key/submit handlers. Pi fires session_start (our
    // bind) from rebindCurrentSession() after those handlers are set up, so
    // the bind is the earliest point at which the TUI reliably accepts input.
    ready_patterns: &[],
    pty: PtySpec {
        delivery_start_timeout_secs: 5,
        console_input: ConsoleInput::Vt,
        pre_enter_settle_ms: 0,
    },
    instance_state_env: &[],
    detection: DetectionSpec {
        markers: &[EnvPredicate::equals("HCOM_PI", "1")],
        clear_for_child: &["PI_CODING_AGENT", "PI_CODING_AGENT_SESSION_DIR"],
    },
    hooks: HooksSpec {
        names: PI_HOOKS,
        shared_hooks_with: None,
        invocation: HookInvocation::Argv,
    },
    // Runtime gates off: the Pi extension owns delivery after bootstrap.
    gates: GatesSpec {
        require_idle: false,
        require_ready_prompt: false,
        require_prompt_empty: false,
        block_on_user_activity: false,
        block_on_approval: true,
        launch_requires_ready: true,
        launch_ready_on_plugin_bind: true,
    },
    launch: LaunchSpec {
        args_env: Some("HCOM_PI_ARGS"),
        config_dir_env: Some("PI_CODING_AGENT_DIR"),
        initial_prompt: InitialPromptShape::Positional,
        uses_pty_default: true,
        max_launch_count: 10,
        background: BackgroundMode::HeadlessPty,
    },
    args: &PI_RESUME_FLAGS,
    resume: Some(ResumeSpec {
        resume: ResumeArgs::Flag("--session"),
        // Pi's `--fork <id>` takes the session id as its value and is mutually
        // exclusive with `--session` ("--fork cannot be combined with
        // --session"). Use Subcommand so build_resume_args emits
        // `["--fork", <id>]` (replacing `--session`), not `["--session", <id>,
        // "--fork"]` which pi rejects.
        fork: Some(ForkArgs::Subcommand("--fork")),
        merge: ResumeMerge::Prepend,
    }),
    help: HelpSpec {
        unique_examples: PI_HELP_EXAMPLES,
        extra_env: &[],
        notes: &[],
    },
    // `file` lists mutating ops only (like every other tool); Pi's `read` is a
    // read-only op and was producing a path in the status bar on plain reads.
    status_detail: StatusDetailSpec {
        bash: &["bash"],
        file: &["edit", "write"],
        delegate: &[],
    },
};

pub static OMP: IntegrationSpec = IntegrationSpec {
    tool: Tool::Omp,
    name: "omp",
    label: "Oh My Pi",
    aliases: &["omp-agent"],
    cli_binary: "omp",
    tui_prefix: "omp ",
    adhoc_icon: None,
    released: true,
    // No usable on-screen readiness marker. Pi's `/ commands` comes from its
    // startup-instructions header; OMP forked that away (WelcomeComponent never
    // renders it — the only `/ commands` in OMP is the first-run theme wizard).
    // Every other candidate lives in the status line, which is preset- and
    // theme-configurable: the minimal/compact/ascii/custom presets omit the pi
    // segment entirely and `icon.pi` is theme-selectable, so anchoring on `π `
    // still falsely blocks established configs. Readiness is instead proven by
    // the hcom extension's bind (`launch_ready_on_plugin_bind`), which is
    // rendering-independent. Empty pattern => is_ready() is always true, so it
    // never gates launch on scraped chrome; the plugin bind is authoritative.
    ready_patterns: &[],
    pty: PtySpec {
        delivery_start_timeout_secs: 5,
        console_input: ConsoleInput::Vt,
        pre_enter_settle_ms: 0,
    },
    instance_state_env: &[],
    detection: DetectionSpec {
        markers: &[EnvPredicate::equals("HCOM_OMP", "1")],
        clear_for_child: &["PI_CODING_AGENT", "PI_CODING_AGENT_SESSION_DIR"],
    },
    hooks: HooksSpec {
        names: OMP_HOOKS,
        shared_hooks_with: None,
        invocation: HookInvocation::Argv,
    },
    gates: GatesSpec {
        require_idle: false,
        require_ready_prompt: false,
        require_prompt_empty: false,
        block_on_user_activity: false,
        block_on_approval: true,
        launch_requires_ready: true,
        // OMP has no reliable on-screen ready marker (all candidates live in the
        // preset/theme-configurable status line), so launch readiness is proven
        // by the hcom extension's bind (kind='plugin' notify endpoint) instead —
        // rendering-independent, and a dead extension correctly fails to bind and
        // blocks. `ready_patterns` is empty; this is the authoritative signal.
        launch_ready_on_plugin_bind: true,
    },
    launch: LaunchSpec {
        args_env: Some("HCOM_OMP_ARGS"),
        config_dir_env: Some("PI_CODING_AGENT_DIR"),
        initial_prompt: InitialPromptShape::Positional,
        uses_pty_default: true,
        max_launch_count: 10,
        background: BackgroundMode::HeadlessPty,
    },
    args: &OMP_RESUME_FLAGS,
    resume: Some(ResumeSpec {
        resume: ResumeArgs::Flag("--resume"),
        // Top-level `omp --help` omits `--fork`, but the flag is real: it has
        // existed and been documented in session-operation docs since v15.1.9.
        // `omp --fork <id>` routes through `SessionManager.forkFrom(...)` (takes
        // the session id/path as its value) and is mutually exclusive with
        // session persistence (`omp --fork x --no-session` -> "--fork requires
        // session persistence"). Same shape as Pi: Subcommand emits
        // `["--fork", <id>]` (replacing `--resume`), not `["--resume", <id>,
        // "--fork"]`. Must not degrade `hcom f` into a plain `--resume`.
        fork: Some(ForkArgs::Subcommand("--fork")),
        merge: ResumeMerge::Prepend,
    }),
    help: HelpSpec {
        unique_examples: OMP_HELP_EXAMPLES,
        extra_env: &[],
        notes: &[],
    },
    // `ast_edit` is left out: it takes glob `paths`, not a file, which would
    // make `--file` filters match patterns instead of paths.
    status_detail: StatusDetailSpec {
        bash: &["bash"],
        file: &["edit", "write"],
        delegate: &["task"],
    },
};

pub static HERMES: IntegrationSpec = IntegrationSpec {
    tool: Tool::Hermes,
    name: "hermes",
    label: "Hermes",
    aliases: &[],
    cli_binary: "hermes",
    tui_prefix: "herm",
    adhoc_icon: None,
    released: true,
    // Readiness is the plugin's bind: the classic REPL and the Ink TUI draw
    // different chrome, and either can be up before the plugin can deliver.
    ready_patterns: &[],
    pty: PtySpec {
        delivery_start_timeout_secs: 5,
        console_input: ConsoleInput::Vt,
        pre_enter_settle_ms: 0,
    },
    instance_state_env: &[],
    detection: DetectionSpec {
        // Hermes sets these in its own env, so its terminal children and the
        // plugin's RPCs carry them. Gateway terminals export HERMES_SESSION_ID
        // empty; HERMES_AGENT still identifies them.
        markers: &[
            EnvPredicate::non_empty("HERMES_SESSION_ID"),
            EnvPredicate::non_empty("HERMES_AGENT"),
        ],
        ..DetectionSpec::NONE
    },
    hooks: HooksSpec {
        names: HERMES_HOOKS,
        shared_hooks_with: None,
        invocation: HookInvocation::Argv,
    },
    // Runtime gates off: the Hermes plugin owns delivery.
    gates: GatesSpec {
        require_idle: false,
        require_ready_prompt: false,
        require_prompt_empty: false,
        block_on_user_activity: false,
        block_on_approval: true,
        launch_requires_ready: true,
        launch_ready_on_plugin_bind: true,
    },
    launch: LaunchSpec {
        args_env: Some("HCOM_HERMES_ARGS"),
        config_dir_env: Some("HERMES_HOME"),
        // A `chat` option: on a TTY it seeds the first interactive turn.
        initial_prompt: InitialPromptShape::Flag("-q"),
        uses_pty_default: true,
        max_launch_count: 10,
        background: BackgroundMode::HeadlessPty,
    },
    args: &HERMES_ARG_FLAGS,
    resume: Some(ResumeSpec {
        resume: ResumeArgs::Flag("--resume"),
        // Hermes has no fork flag (only an interactive /branch).
        fork: None,
        merge: ResumeMerge::Append,
    }),
    help: HelpSpec {
        unique_examples: HERMES_HELP_EXAMPLES,
        extra_env: &[],
        notes: HERMES_HELP_NOTES,
    },
    status_detail: StatusDetailSpec {
        bash: &["terminal"],
        file: &["write_file", "patch"],
        delegate: &["delegate_task"],
    },
};

pub static COPILOT: IntegrationSpec = IntegrationSpec {
    tool: Tool::Copilot,
    name: "copilot",
    label: "Copilot",
    aliases: &[],
    cli_binary: "copilot",
    tui_prefix: "cop ",
    adhoc_icon: None,
    released: true,
    // Copilot fires SessionStart twice: once at boot and again after it loads
    // hooks/instructions. Gate on "/ commands" footer text so delivery doesn't
    // inject during the loading window.
    ready_patterns: &["/ commands"],
    // Closed-source; unknown instance-state vars are a documented gap.
    pty: PtySpec {
        delivery_start_timeout_secs: 60,
        console_input: ConsoleInput::Vt,
        pre_enter_settle_ms: 0,
    },
    // hcom's per-launch delivery mode; a nested `hcom copilot` decides its own.
    instance_state_env: &["HCOM_COPILOT_EXT_DELIVERY"],
    detection: DetectionSpec::NONE,
    hooks: HooksSpec {
        names: COPILOT_HOOKS,
        shared_hooks_with: None,
        invocation: HookInvocation::JsonStdin,
    },
    gates: GatesSpec {
        require_idle: true,
        require_ready_prompt: true,
        require_prompt_empty: true,
        block_on_user_activity: true,
        block_on_approval: true,
        launch_requires_ready: true,
        launch_ready_on_plugin_bind: false,
    },
    launch: LaunchSpec {
        args_env: Some("HCOM_COPILOT_ARGS"),
        config_dir_env: Some("COPILOT_HOME"),
        initial_prompt: InitialPromptShape::Flag("-i"),
        uses_pty_default: true,
        max_launch_count: 10,
        background: BackgroundMode::HeadlessPty,
    },
    args: &COPILOT_RESUME_FLAGS,
    resume: Some(ResumeSpec {
        resume: ResumeArgs::Flag("--resume"),
        fork: None,
        merge: ResumeMerge::Prepend,
    }),
    help: HelpSpec {
        unique_examples: COPILOT_HELP_EXAMPLES,
        extra_env: &[],
        notes: &[],
    },
    status_detail: StatusDetailSpec {
        bash: &["bash", "powershell"],
        file: &["create", "edit", "apply_patch"],
        delegate: &["task"],
    },
};

pub static QODER: IntegrationSpec = IntegrationSpec {
    tool: Tool::Qoder,
    name: "qoder",
    label: "Qoder",
    aliases: &["qodercli"],
    cli_binary: "qodercli",
    tui_prefix: "qod ",
    adhoc_icon: None,
    released: true,
    // Idle composer placeholder, or the `? for shortcuts` footer hint. The
    // hint is sometimes replaced by a banner (e.g. credits exhausted), so
    // neither marker can stand alone.
    ready_patterns: &["Type your message", "? for shortcuts"],
    pty: PtySpec {
        delivery_start_timeout_secs: 60,
        console_input: ConsoleInput::Vt,
        // Qoder 1.1.65 submits the text twice when Enter follows it within a
        // few tens of ms as a separate write; 100ms measured, plus margin.
        pre_enter_settle_ms: 250,
    },
    // Qoder's own session env vars are read as input on a fresh start and are
    // instance-specific. `QODER_CONFIG_DIR` is deliberately not here: it is a
    // user config toggle (see `config_dir_env`).
    instance_state_env: &[
        "QODER_SESSION_ID",
        "QODER_SESSION_NAME",
        "QODER_WORKING_DIR",
    ],
    detection: DetectionSpec {
        // Set in its Bash tool. Its hook processes also get CLAUDE_PROJECT_DIR,
        // which is deliberately not a Claude marker.
        markers: &[EnvPredicate::set("QODER_CLI")],
        clear_for_child: &["QODER_PID"],
    },
    hooks: HooksSpec {
        names: QODER_HOOKS,
        shared_hooks_with: None,
        invocation: HookInvocation::JsonStdin,
    },
    gates: GatesSpec {
        require_idle: true,
        require_ready_prompt: true,
        require_prompt_empty: true,
        block_on_user_activity: true,
        block_on_approval: true,
        launch_requires_ready: true,
        launch_ready_on_plugin_bind: false,
    },
    launch: LaunchSpec {
        args_env: Some("HCOM_QODER_ARGS"),
        config_dir_env: Some("QODER_CONFIG_DIR"),
        initial_prompt: InitialPromptShape::Flag("--prompt-interactive"),
        uses_pty_default: true,
        max_launch_count: 10,
        background: BackgroundMode::HeadlessPty,
    },
    args: &QODER_RESUME_FLAGS,
    resume: Some(ResumeSpec {
        resume: ResumeArgs::Flag("--resume"),
        fork: Some(ForkArgs::AppendFlag("--fork-session")),
        merge: ResumeMerge::Prepend,
    }),
    help: HelpSpec {
        unique_examples: QODER_HELP_EXAMPLES,
        extra_env: &[],
        notes: &[],
    },
    status_detail: StatusDetailSpec {
        bash: &["Bash"],
        file: &["Write", "Edit"],
        delegate: &["Agent"],
    },
};

pub static GROK: IntegrationSpec = IntegrationSpec {
    tool: Tool::Grok,
    name: "grok",
    label: "Grok Build",
    aliases: &["grok-build"],
    cli_binary: "grok",
    tui_prefix: "grk ",
    adhoc_icon: None,
    released: true,
    // Delivery goes through Grok's prompt queue (delivery/grok.rs), never the
    // PTY, so there is no composer to parse and no ready footer to wait for.
    ready_patterns: &[],
    pty: PtySpec {
        delivery_start_timeout_secs: 10,
        console_input: ConsoleInput::Vt,
        pre_enter_settle_ms: 0,
    },
    // Session id is instance-specific and would corrupt a same-tool child launch.
    instance_state_env: &["GROK_SESSION_ID"],
    detection: DetectionSpec {
        markers: &[
            EnvPredicate::set("GROK_SESSION_ID"),
            EnvPredicate::set("GROK_HOOK_EVENT"),
        ],
        clear_for_child: &["GROK_HOOK_NAME", "GROK_AGENT", "GROK_LEADER_SOCKET"],
    },
    hooks: HooksSpec {
        names: GROK_HOOKS,
        shared_hooks_with: None,
        invocation: HookInvocation::JsonStdin,
    },
    gates: GatesSpec {
        require_idle: true,
        require_ready_prompt: false,
        require_prompt_empty: false,
        block_on_user_activity: true,
        block_on_approval: true,
        // Launch readiness falls back to the settle timeout.
        launch_requires_ready: false,
        launch_ready_on_plugin_bind: false,
    },
    launch: LaunchSpec {
        args_env: Some("HCOM_GROK_ARGS"),
        config_dir_env: Some("GROK_HOME"),
        initial_prompt: InitialPromptShape::DashDashPositional,
        uses_pty_default: true,
        max_launch_count: 10,
        background: BackgroundMode::HeadlessPty,
    },
    args: &GROK_RESUME_FLAGS,
    resume: Some(ResumeSpec {
        resume: ResumeArgs::Flag("--resume"),
        fork: Some(ForkArgs::AppendFlag("--fork-session")),
        merge: ResumeMerge::Prepend,
    }),
    help: HelpSpec {
        unique_examples: GROK_HELP_EXAMPLES,
        extra_env: &[],
        notes: &[],
    },
    status_detail: StatusDetailSpec {
        bash: &["run_terminal_command", "Bash"],
        file: &["search_replace", "write", "Edit", "Write", "MultiEdit"],
        delegate: &["spawn_subagent", "Task"],
    },
};

pub static ADHOC: IntegrationSpec = IntegrationSpec {
    tool: Tool::Adhoc,
    name: "adhoc",
    label: "Adhoc",
    aliases: &[],
    cli_binary: "",
    tui_prefix: "ah  ",
    adhoc_icon: Some("\u{25e6}"), // ◦ neutral dot
    released: false,
    ready_patterns: &[],
    pty: PtySpec {
        delivery_start_timeout_secs: 60,
        console_input: ConsoleInput::Vt,
        pre_enter_settle_ms: 0,
    },
    instance_state_env: &[],
    detection: DetectionSpec::NONE,
    hooks: HooksSpec {
        names: &[],
        shared_hooks_with: None,
        invocation: HookInvocation::None,
    },
    // Adhoc delivery is manual via `hcom listen` / hookless CLI unread, so
    // these conservative gates are documented but mostly unused.
    gates: GatesSpec {
        require_idle: true,
        require_ready_prompt: false,
        require_prompt_empty: true,
        block_on_user_activity: true,
        block_on_approval: true,
        launch_requires_ready: false,
        launch_ready_on_plugin_bind: false,
    },
    launch: LaunchSpec {
        args_env: None,
        config_dir_env: None,
        initial_prompt: InitialPromptShape::Positional,
        uses_pty_default: false,
        max_launch_count: 0,
        background: BackgroundMode::Unsupported,
    },
    args: &ArgFlags::EMPTY,
    resume: None,
    help: HelpSpec {
        unique_examples: &[],
        extra_env: &[],
        notes: &[],
    },
    status_detail: StatusDetailSpec {
        bash: &[],
        file: &[],
        delegate: &[],
    },
};

/// All specs in canonical order. `Tool::spec` indexes into this implicitly via
/// match — exposed for iteration (released-list helpers, hook-name routing).
pub static ALL: &[&IntegrationSpec] = &[
    &CLAUDE,
    &GEMINI,
    &CODEX,
    &OPENCODE,
    &KILO,
    &PI,
    &OMP,
    &HERMES,
    &ANTIGRAVITY,
    &CURSOR,
    &KIMI,
    &COPILOT,
    &QODER,
    &GROK,
    &ADHOC,
];

impl Tool {
    /// Return this tool's integration spec.
    pub fn spec(self) -> &'static IntegrationSpec {
        match self {
            Tool::Claude => &CLAUDE,
            Tool::Gemini => &GEMINI,
            Tool::Codex => &CODEX,
            Tool::OpenCode => &OPENCODE,
            Tool::Kilo => &KILO,
            Tool::Omp => &OMP,
            Tool::Pi => &PI,
            Tool::Hermes => &HERMES,
            Tool::Antigravity => &ANTIGRAVITY,
            Tool::Cursor => &CURSOR,
            Tool::Kimi => &KIMI,
            Tool::Copilot => &COPILOT,
            Tool::Qoder => &QODER,
            Tool::Grok => &GROK,
            Tool::Adhoc => &ADHOC,
        }
    }
}

/// Tool names in the public `RELEASED_TOOLS` set, computed from specs.
pub fn released_tool_names() -> Vec<&'static str> {
    ALL.iter().filter(|s| s.released).map(|s| s.name).collect()
}

/// Tool names supporting background/headless launch via the tool's own
/// detached print mode (currently only Claude `-p`). PTY-wrapped headless
/// (`HeadlessPty`) is universal across released tools and not reported here —
/// this set drives the larger `max_launch_count` budget given to true detached
/// bulk launches.
pub fn released_background_tool_names() -> Vec<&'static str> {
    ALL.iter()
        .filter(|s| s.released && matches!(s.launch.background, BackgroundMode::NativePrint))
        .map(|s| s.name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn integrations_conform_across_registries() {
        use crate::config::{HcomConfig, user_field};
        use crate::launcher::LaunchTool;
        use crate::shared::tool_detection::detect_tool;
        use crate::tui::model::{next_launch_tool, prev_launch_tool};
        use std::collections::{HashMap, HashSet};

        let mut names = HashMap::new();
        let mut forward = HashSet::new();
        let mut backward = HashSet::new();
        for (seen, step) in [
            (&mut forward, next_launch_tool as fn(Tool) -> Tool),
            (&mut backward, prev_launch_tool as fn(Tool) -> Tool),
        ] {
            let mut current = Tool::Claude;
            while seen.insert(current) {
                current = step(current);
            }
            assert_eq!(current, Tool::Claude, "TUI cycle did not close");
        }

        for spec in ALL {
            for name in std::iter::once(&spec.name).chain(spec.aliases.iter()) {
                assert_eq!(
                    names.insert(*name, spec.tool),
                    None,
                    "{}: duplicate name or alias {name}",
                    spec.name
                );
                assert_eq!(
                    name.parse::<Tool>().ok(),
                    Some(spec.tool),
                    "{}: Tool cannot parse {name}",
                    spec.name
                );
                if spec.tool != Tool::Adhoc {
                    assert_eq!(
                        LaunchTool::from_str(name).ok().map(|t| t.tool()),
                        Some(spec.tool),
                        "{}: LaunchTool cannot parse {name}",
                        spec.name
                    );
                }
            }
            for hook in spec.hooks.names {
                assert_eq!(
                    Tool::from_hook_name(hook),
                    Some(spec.hooks.shared_hooks_with.unwrap_or(spec.tool)),
                    "{}: hook {hook} routes to wrong tool",
                    spec.name
                );
            }
            if let Some(env) = spec.launch.args_env {
                let key = format!("{}_args", spec.name);
                let field = user_field(&key)
                    .unwrap_or_else(|| panic!("{}: missing config key {key}", spec.name));
                assert_eq!(field.env, env, "{}: args env mapping", spec.name);
                assert_eq!(
                    field.toml,
                    format!("launch.{}.args", spec.name),
                    "{}: args TOML mapping",
                    spec.name
                );
                let mut config = HcomConfig::default();
                config.set_field(&key, "--model 'test model'").unwrap();
                assert_eq!(
                    config.get_field(&key).as_deref(),
                    Some("--model 'test model'"),
                    "{}: args get/set round-trip",
                    spec.name
                );
                assert_eq!(
                    config.tool_args(spec.tool),
                    "--model 'test model'",
                    "{}: args accessor",
                    spec.name
                );
                assert_eq!(
                    config.to_env_dict().get(env).map(String::as_str),
                    Some("--model 'test model'"),
                    "{}: args env export",
                    spec.name
                );
                let vars = HashMap::from([(env.to_string(), "--model from-env".to_string())]);
                let (loaded, errors) = HcomConfig::load_from_sources(&HashMap::new(), Some(&vars));
                assert!(
                    errors.is_empty(),
                    "{}: args env load errors {errors:?}",
                    spec.name
                );
                assert_eq!(
                    loaded.tool_args(spec.tool),
                    "--model from-env",
                    "{}: args env load",
                    spec.name
                );
                let content = format!("[launch.{}]\nargs = \"--model from-toml\"\n", spec.name);
                let (values, errors) = crate::config::toml_values_from_str(&content);
                assert!(
                    errors.is_empty(),
                    "{}: args TOML parse errors {errors:?}",
                    spec.name
                );
                let (loaded, errors) =
                    HcomConfig::load_from_sources(&values, Some(&HashMap::new()));
                assert!(
                    errors.is_empty(),
                    "{}: args TOML load errors {errors:?}",
                    spec.name
                );
                assert_eq!(
                    loaded.tool_args(spec.tool),
                    "--model from-toml",
                    "{}: args TOML load",
                    spec.name
                );
                config.set_field(&key, "'unclosed").unwrap();
                assert!(
                    config.collect_errors().contains_key(&key),
                    "{}: args quoting validation missing",
                    spec.name
                );
                assert!(
                    crate::commands::config::config_help(env).is_some(),
                    "{}: args help missing",
                    spec.name
                );
            }
            if spec.tool == Tool::Adhoc {
                continue;
            }
            assert!(
                forward.contains(&spec.tool),
                "{}: missing from next TUI cycle",
                spec.name
            );
            assert!(
                backward.contains(&spec.tool),
                "{}: missing from prev TUI cycle",
                spec.name
            );
            assert_eq!(
                detect_tool(&HashMap::from([(
                    "HCOM_TOOL".to_string(),
                    spec.name.to_string()
                )])),
                spec.tool,
                "{}: HCOM_TOOL detection failed",
                spec.name
            );
            let expected_per_run = match spec.tool {
                Tool::Claude
                | Tool::Codex
                | Tool::OpenCode
                | Tool::Kilo
                | Tool::Pi
                | Tool::Omp
                | Tool::Copilot
                | Tool::Qoder
                | Tool::Grok => true,
                Tool::Gemini | Tool::Antigravity | Tool::Cursor | Tool::Kimi | Tool::Hermes => {
                    false
                }
                Tool::Adhoc => unreachable!(),
            };
            let per_run = crate::hooks::runtime::adapter(spec.tool).is_some();
            assert_eq!(
                per_run, expected_per_run,
                "{}: runtime adapter registration disagrees with hook lifecycle",
                spec.name
            );
            let persistent = spec.tool.hook_installer().is_some();
            assert_ne!(
                per_run, persistent,
                "{}: must have exactly one runtime adapter or persistent verify/setup registration",
                spec.name
            );
        }
    }

    #[test]
    fn resume_flag_tables_are_canonical() {
        for spec in ALL.iter().filter(|s| s.tool != Tool::Adhoc) {
            let resume = spec
                .resume
                .as_ref()
                .unwrap_or_else(|| panic!("{}: missing resume spec", spec.name));
            let flags = spec.args;
            let selectors = [
                match resume.resume {
                    ResumeArgs::Flag(flag) => Some(flag),
                    ResumeArgs::Subcommand(_) => None,
                },
                match resume.fork {
                    Some(ForkArgs::AppendFlag(flag)) => Some(flag),
                    Some(ForkArgs::Subcommand(_)) => None,
                    None => None,
                },
            ];
            for selector in selectors.into_iter().flatten() {
                assert!(
                    flags.drop.contains(&selector),
                    "{}: resume flags must drop a saved {selector}",
                    spec.name
                );
            }
            let tables = [
                flags.value,
                flags.multi_value,
                flags.boolean,
                flags.drop,
                flags.repeatable,
            ];
            for name in tables.iter().flat_map(|t| t.iter()) {
                assert!(
                    !flags.aliases.iter().any(|(short, _)| short == name),
                    "{}: resume flag table lists alias {name}; use the long form",
                    spec.name
                );
            }
            for name in flags.repeatable {
                assert!(
                    flags.value.contains(name) || flags.multi_value.contains(name),
                    "{}: repeatable {name} has no value arity",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn kimi_initial_prompt_is_explicitly_unsupported() {
        assert!(matches!(
            KIMI.launch.initial_prompt,
            InitialPromptShape::Unsupported { .. }
        ));
    }

    #[test]
    fn every_tool_variant_has_a_spec() {
        for tool in [
            Tool::Claude,
            Tool::Gemini,
            Tool::Codex,
            Tool::OpenCode,
            Tool::Kilo,
            Tool::Antigravity,
            Tool::Cursor,
            Tool::Kimi,
            Tool::Copilot,
            Tool::Qoder,
            Tool::Grok,
            Tool::Pi,
            Tool::Omp,
            Tool::Hermes,
            Tool::Adhoc,
        ] {
            let spec = tool.spec();
            assert_eq!(spec.tool, tool, "spec.tool mismatch for {tool:?}");
        }
    }

    #[test]
    fn spec_name_round_trips_through_from_str() {
        for spec in ALL {
            let parsed: Tool = spec.name.parse().expect("parses");
            assert_eq!(parsed, spec.tool);
        }
    }

    #[test]
    fn hook_names_disjoint_among_primary_specs() {
        let mut seen: HashSet<&'static str> = HashSet::new();
        for spec in ALL {
            if spec.hooks.shared_hooks_with.is_some() {
                continue;
            }
            for name in spec.hooks.names {
                assert!(
                    seen.insert(*name),
                    "{} is owned by more than one routing spec",
                    name
                );
            }
        }
    }

    #[test]
    fn antigravity_hooks_match_gemini() {
        assert_eq!(ANTIGRAVITY.hooks.names, GEMINI.hooks.names);
        assert_eq!(ANTIGRAVITY.hooks.shared_hooks_with, Some(Tool::Gemini));
    }

    #[test]
    fn kilo_hooks_match_opencode() {
        assert_eq!(KILO.hooks.names, OPENCODE.hooks.names);
        assert_eq!(KILO.hooks.shared_hooks_with, Some(Tool::OpenCode));
    }

    #[test]
    fn adhoc_is_quiet() {
        assert!(ADHOC.hooks.names.is_empty());
        assert_eq!(ADHOC.hooks.invocation, HookInvocation::None);
        assert!(ADHOC.resume.is_none());
        assert!(!ADHOC.released);
    }

    #[test]
    fn released_tools_matches_prior_constant() {
        let names = released_tool_names();
        assert!(names.contains(&"claude"));
        assert!(names.contains(&"gemini"));
        assert!(names.contains(&"codex"));
        assert!(names.contains(&"opencode"));
        assert!(names.contains(&"kilo"));
        assert!(names.contains(&"pi"));
        assert!(names.contains(&"antigravity"));
        assert!(names.contains(&"cursor"));
        assert!(names.contains(&"kimi"));
        assert!(names.contains(&"copilot"));
        assert!(names.contains(&"qoder"));
        assert!(names.contains(&"grok"));
        assert!(names.contains(&"omp"));
        assert!(names.contains(&"hermes"));
        assert_eq!(names.len(), 14);
    }

    #[test]
    fn released_background_matches_prior_constant() {
        assert_eq!(released_background_tool_names(), vec!["claude"]);
    }

    #[test]
    fn every_tool_declares_a_nonzero_pty_delivery_timeout() {
        for spec in ALL {
            assert!(
                spec.pty.delivery_start_timeout_secs > 0,
                "{} must declare a PTY delivery timeout",
                spec.name
            );
        }
        assert_eq!(GEMINI.pty.delivery_start_timeout_secs, 60);
        assert_eq!(COPILOT.pty.delivery_start_timeout_secs, 60);
        assert_eq!(CLAUDE.pty.delivery_start_timeout_secs, 5);
    }

    #[test]
    fn claude_uses_pty_default_false() {
        assert!(!CLAUDE.launch.uses_pty_default);
        for spec in ALL {
            if spec.tool == Tool::Claude || spec.tool == Tool::Adhoc {
                continue;
            }
            assert!(
                spec.launch.uses_pty_default,
                "{} should default to PTY",
                spec.name
            );
        }
    }

    #[test]
    fn aliases_resolve_to_owning_spec() {
        for spec in ALL {
            for alias in spec.aliases {
                let parsed: Tool = alias.parse().expect("alias should parse");
                assert_eq!(
                    parsed, spec.tool,
                    "alias {} resolved to {:?}, expected {:?}",
                    alias, parsed, spec.tool
                );
            }
        }
    }

    #[test]
    fn aliases_are_globally_unique() {
        let mut seen: HashSet<&'static str> = HashSet::new();
        for spec in ALL {
            assert!(
                seen.insert(spec.name),
                "name {} is duplicated across specs",
                spec.name
            );
            for alias in spec.aliases {
                assert!(
                    seen.insert(*alias),
                    "alias {} collides with another tool name or alias",
                    alias
                );
            }
        }
    }

    #[test]
    fn released_specs_have_resume_and_cli_binary() {
        for spec in ALL {
            if !spec.released {
                continue;
            }
            assert!(
                !spec.cli_binary.is_empty(),
                "released tool {} must have a cli_binary",
                spec.name
            );
            assert!(
                spec.resume.is_some(),
                "released tool {} must define a resume spec (use None only for Adhoc)",
                spec.name
            );
        }
    }

    #[test]
    fn max_launch_count_only_zero_for_unreleased() {
        for spec in ALL {
            if spec.released {
                assert!(
                    spec.launch.max_launch_count > 0,
                    "released tool {} must have max_launch_count > 0",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn max_launch_count_matches_background_capability() {
        // Only claude supports headless launch today; if that ever changes,
        // released_background_tool_names() and max_launch_count budgets should
        // grow together — flag the assumption here.
        assert_eq!(released_background_tool_names(), vec!["claude"]);
        assert_eq!(CLAUDE.launch.max_launch_count, 100);
        for spec in ALL {
            if spec.tool != Tool::Claude && spec.released {
                assert_eq!(
                    spec.launch.max_launch_count, 10,
                    "{} should match the non-background cap of 10",
                    spec.name
                );
            }
        }
    }

    // ── Released-tool drift gate ────────────────────────────────────────
    //
    // Every released tool must clear each gate below or document an explicit
    // opt-out in its spec (e.g. Antigravity's `args_env: None` opts out of
    // HCOM_*_ARGS config; Gemini/Antigravity's `resume.fork = None` opts out
    // of `hcom f`). Adding a new released tool means filling these surfaces
    // or pinning the opt-out here next to the existing carve-outs.

    #[test]
    fn drift_released_tools_resolve_via_launch_tool() {
        // Adding a released tool must wire it into LaunchTool::from_str so
        // `hcom <tool>` reaches the launcher. Each canonical name AND each
        // alias must parse and resolve back to the owning Tool variant.
        use crate::launcher::LaunchTool;
        for spec in ALL {
            if !spec.released {
                continue;
            }
            let lt = LaunchTool::from_str(spec.name)
                .unwrap_or_else(|e| panic!("LaunchTool::from_str({}) failed: {e}", spec.name));
            assert_eq!(
                lt.tool(),
                spec.tool,
                "{}: LaunchTool::tool() did not resolve to owning Tool",
                spec.name
            );
            assert_eq!(
                lt.cli_binary(),
                spec.cli_binary,
                "{}: LaunchTool::cli_binary() must match spec",
                spec.name
            );
            for alias in spec.aliases {
                let lt = LaunchTool::from_str(alias)
                    .unwrap_or_else(|e| panic!("LaunchTool::from_str alias {alias} failed: {e}"));
                assert_eq!(
                    lt.tool(),
                    spec.tool,
                    "alias {alias} did not resolve to owning Tool"
                );
            }
        }
    }

    #[test]
    fn drift_released_tools_have_help_referencing_label() {
        // Every released tool — canonical name and each alias — must produce
        // launch help that starts with "Usage:" and references the spec's
        // human-readable label. Generated by commands::help::generate_tool_help
        // from the spec itself, so failure here means a spec field that
        // affects help rendering was renamed without updating the template.
        use crate::commands::help::get_command_help;
        for spec in ALL {
            if !spec.released {
                continue;
            }
            let help = get_command_help(spec.name);
            assert!(
                help.starts_with("Usage:"),
                "help for {} must start with 'Usage:'",
                spec.name
            );
            let label_marker = format!("Launch N {} agents", spec.label);
            assert!(
                help.contains(&label_marker),
                "help for {} must reference label '{}' (looked for '{}'):\n{}",
                spec.name,
                spec.label,
                label_marker,
                help
            );
            for alias in spec.aliases {
                let alias_help = get_command_help(alias);
                assert!(
                    alias_help.starts_with("Usage:"),
                    "alias help for {alias} must start with 'Usage:'"
                );
            }
        }
    }

    #[test]
    fn drift_released_tools_args_env_documented_in_config() {
        // args_env opt-out is allowed (Antigravity is None today); when set,
        // the env-var name must point at a real HcomConfig field. The mapping
        // table lives in config.rs and is asserted in
        // `args_env_keys_match_integration_specs` — this gate just pins the
        // expectation that each released spec either sets args_env to a
        // non-empty string or explicitly opts out via None.
        for spec in ALL {
            if !spec.released {
                continue;
            }
            if let Some(env_var) = spec.launch.args_env {
                assert!(
                    env_var.starts_with("HCOM_") && env_var.ends_with("_ARGS"),
                    "{}: args_env '{}' must follow HCOM_*_ARGS naming",
                    spec.name,
                    env_var
                );
            }
        }
    }

    #[test]
    fn drift_released_tools_with_args_env_merge_their_config_field() {
        use crate::commands::launch::merge_tool_args;
        use crate::config::HcomConfig;
        use crate::launcher::LaunchTool;

        for spec in ALL {
            let Some(args_env) = spec.launch.args_env else {
                continue;
            };
            if !spec.released {
                continue;
            }

            let field = args_env
                .strip_prefix("HCOM_")
                .expect("args env uses HCOM_ prefix")
                .to_ascii_lowercase();
            let mut config = HcomConfig::default();
            config
                .set_field(&field, "--model config-model")
                .unwrap_or_else(|e| panic!("{} config field {field}: {e}", spec.name));
            let launch_tool = LaunchTool::from_str(spec.name).unwrap();
            let merged = merge_tool_args(&launch_tool, &[], &config);

            assert!(
                merged.iter().any(|arg| arg == "config-model"),
                "{} must consume config field {field} declared by {args_env}",
                spec.name
            );
        }
    }

    #[test]
    fn drift_released_tools_have_background_mode() {
        // Every released tool must declare a background mode other than
        // Unsupported. Unsupported is reserved for Adhoc, which is never
        // launched through `hcom [N] <tool>`.
        for spec in ALL {
            if spec.released {
                assert_ne!(
                    spec.launch.background,
                    BackgroundMode::Unsupported,
                    "{}: released tool must declare a BackgroundMode other than Unsupported",
                    spec.name
                );
            } else {
                assert_eq!(
                    spec.launch.background,
                    BackgroundMode::Unsupported,
                    "{}: unreleased tool must declare BackgroundMode::Unsupported",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn drift_native_print_implies_large_launch_budget() {
        // The 100-agent budget exists because NativePrint background is
        // truly detached (no terminal cost). HeadlessPty caps at 10 because
        // each instance still consumes a runner. If a new tool gains
        // NativePrint, bump max_launch_count to match Claude or update this
        // gate.
        for spec in ALL {
            if !spec.released {
                continue;
            }
            match spec.launch.background {
                BackgroundMode::NativePrint => {
                    assert_eq!(
                        spec.launch.max_launch_count, 100,
                        "{}: NativePrint background expects a 100-agent budget",
                        spec.name
                    );
                }
                BackgroundMode::HeadlessPty => {
                    assert_eq!(
                        spec.launch.max_launch_count, 10,
                        "{}: HeadlessPty background expects a 10-agent budget",
                        spec.name
                    );
                }
                BackgroundMode::Unsupported => {
                    unreachable!(
                        "released tools handled by drift_released_tools_have_background_mode"
                    )
                }
            }
        }
    }

    #[test]
    fn command_names_covers_released_tools() {
        use crate::commands::help::COMMAND_NAMES;
        for spec in ALL {
            if !spec.released {
                continue;
            }
            assert!(
                COMMAND_NAMES.contains(&spec.name),
                "COMMAND_NAMES missing released tool {}",
                spec.name
            );
            for alias in spec.aliases {
                assert!(
                    COMMAND_NAMES.contains(alias),
                    "COMMAND_NAMES missing alias {} for released tool {}",
                    alias,
                    spec.name
                );
            }
        }
    }
}
