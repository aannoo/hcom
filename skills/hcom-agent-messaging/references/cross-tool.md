# Cross-Tool Patterns: Claude + Codex + Gemini + OpenCode + Kilo Code + Pi + OMP + Antigravity + Cursor + Kimi + Copilot + Grok

Verified behavior when mixing different AI coding tools via hcom.

## Typical combos

- **worker + reviewer across tools** — one tool implements, another reviews. Catches blind spots from training-data overlap.
- **sandboxed executor** — Codex runs tests or touches risky files; Claude or Gemini orchestrates from outside the sandbox.
- **diverse answers, one judge** — fan out the same question to multiple tools, one agent reads all transcripts and picks.

## Per-Tool Technical Details

### Claude Code
- **Hooks**: SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, Stop, PermissionRequest, SubagentStart, SubagentStop, Notification, SessionEnd
- **Payload**: JSON via stdin
- **Exit codes**: 0=allow, 2=block with message delivery
- **Session binding**: On SessionStart hook, immediate
- **Message delivery**: Hook output in `additionalContext`
- **Headless mode**: `-p` (print) flag for background, `setsid()` detach
- **Subagent support**: Yes, via Task with background=true
- **Bootstrap injection**: On SessionStart, includes command reference, active agents, scripts

### Codex
- **Hooks**: SessionStart, UserPromptSubmit, PreToolUse (Bash), PostToolUse (Bash), Stop
- **Payload**: JSON via stdin
- **Session binding**: On SessionStart hook, immediate (same as Claude)
- **Message delivery**: Hook-based auto-delivery when hcom-launched; PTY injection fallback for vanilla sessions
- **Sandbox modes**: `workspace` (--full-auto + network), `untrusted` (--sandbox workspace-write), `danger-full-access` (--dangerously-bypass-approvals-and-sandbox), `none` (raw)
- **Bootstrap injection**: Via `-c developer_instructions=<bootstrap>` at launch time
- **Transcript path**: Derived from thread ID, searched via glob in `$CODEX_HOME/sessions/`

### Gemini CLI
- **Hooks**: sessionstart, beforeagent, afteragent, beforetool, aftertool, notification, sessionend
- **Payload**: JSON via stdin
- **Session binding**: On beforeagent hook
- **Message delivery**: Hook output
- **System prompt**: Written to `~/.hcom/system-prompts/gemini.md`, set via `GEMINI_SYSTEM_MD` env var
- **Policy auto-approval**: `~/.gemini/policies/hcom.toml`
- **Transcript path**: Derived from session_id, searched in `~/.gemini/chats/`

### OpenCode
- **Hooks**: start, status, read, stop — via TypeScript plugin
- **Plugin location**: `$XDG_CONFIG_HOME/opencode/plugins/hcom.ts`
- **Session binding**: Via TCP binding ceremony (plugin calls `hcom opencode-start --session-id`)
- **Message delivery**: Plugin TCP endpoint
- **Auto-approval**: `OPENCODE_PERMISSION` env var scoped to safe hcom command prefixes

### Kilo Code

- **Delivery**: Shares OpenCode's `hcom.ts` plugin and `opencode-*` hook handlers.
- **Plugin location**: `$XDG_CONFIG_HOME/kilo/plugins/hcom.ts`
- **Session binding**: Via the OpenCode-family TCP binding ceremony.
- **Transcript**: SQLite at `$XDG_DATA_HOME/kilo/kilo.db` unless `KILO_DB` overrides it.
- **Resume/Fork**: `--session <id>` / `--session <id> --fork`
- **Message delivery**: Plugin TCP endpoint
- **Auto-approval**: `KILO_PERMISSION` env var scoped to safe hcom command prefixes

### Cursor (cursor-agent)
- **Hooks**: sessionStart, beforeSubmitPrompt, preToolUse, postToolUse, stop, sessionEnd
- **Payload**: JSON via stdin
- **Session binding**: On sessionStart hook, immediate
- **Message delivery**: Hook-based when hcom-launched. Active turn → body in postToolUse `additional_context`. Idle agent → a sentinel turn whose `stop` hook carries the body in `followup_message` (one extra turn vs. other tools — the idle wake costs a round-trip).
- **Background mode**: HeadlessPty — runs under a PTY even when headless (cursor-agent `--print` drops the beforeSubmitPrompt + stop hooks, so hcom keeps the interactive TUI). No detached `--print` background like Claude.
- **Approval handling**: cursor's interactive approval prompt ("Run this command?") is detected by PTY screen scrape; a message held at an approval surfaces status `blocked: approval pending`.
- **Status detail**: edit tool is `StrReplace` (not `Edit`); file/edit tools key the path off `path` (not `file_path`); shell has the `run_terminal_cmd` variant; delegates are `Task`/`Subagent`.
- **Fork**: not supported (cursor-agent has no native branch primitive — only `--resume`/`--continue`); resume preserved.
- **Transcript**: cursor-agent writes JSONL under `~/.cursor/projects/<slug>/agent-transcripts/<uuid>/<uuid>.jsonl`. Parser support is limited: no timestamps, `cwd`, or tool-result blocks; user prompts require wrapper removal.

### Grok Build
- **Hooks**: SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, Stop, SubagentStart, SubagentStop, SessionEnd — native `~/.grok/hooks/hcom.json` (`$GROK_HOME`).
- **Payload**: JSON via stdin. Observe events discard stdout; only Stop / SubagentStop parse `hookSpecificOutput.additionalContext`.
- **Bootstrap**: launch `--rules` (system prompt append). SessionStart does not inject bootstrap.
- **Message delivery**: managed `hcom grok` and `hcom grok --headless` sessions use a thin native ACP client connected to the same Grok leader/session. Message bodies enter Grok's native queue; no composer wake text or Enter is injected. Stop observes activity but does not consume the ACP-owned mailbox. Unmanaged hook-only sessions retain Stop additionalContext delivery.
- **Delivery confirmation**: only a matching successful `end_turn` advances the existing mailbox cursor. Queue notifications alone do not acknowledge delivery. Cancellation, failed/ambiguous writes, disconnect, or a session change during delivery retain unread mail and stop automatic replay (`acp_unacknowledged`); no input-box fallback occurs. Inspect the session before restarting, since an interrupted transport may already have delivered the message.
- **Composer and permissions**: the native TUI retains its draft/cursor and handles permission prompts. hcom never auto-approves or auto-rejects ACP permission requests. Ordinary managed sessions work with stock Grok; a custom build is not required. The final Windows integration was tested with official Grok 1.0.13: real hcom replies, multiline draft/mid-text cursor preservation through a permission dialog, busy queue, unattended headless wake, and handled transport interruption.
- **Startup restrictions**: hcom owns the leader endpoint. `--no-subagents` is forwarded as native `GROK_SUBAGENTS=0`. Optional `--allow/--deny` (including aliases) and `--disable-web-search` require native `agent leader --launch-policy` support; hcom checks that capability only when those flags are requested. Official Grok 1.0.13 rejects these restricted launches before an agent is created, rather than silently losing restrictions. Ordinary launches do not run this capability probe. On a capable binary, both TUI and ACP receive the same restriction arguments; no hcom policy store is added.
- **Native safety gates**: allow rules do not bypass Grok's Bash/shell preflight. PowerShell call-operator syntax (`& "program"`) can still require native confirmation when the Bash parser cannot safely decompose it. hcom does not override that decision.
- **One-shot**: `hcom grok -p` / `--single` is rejected — process exits and cannot stay on the bus.
- **Resume**: `--resume` / `--fork-session`. Worktree flags from the original launch are not replayed.
- **Transcript**: `$GROK_HOME/sessions/**/updates.jsonl` (default `~/.grok/sessions`).

## Working Patterns

See `scripts/cross-tool-duo.sh` for Claude architect + Codex engineer, and `scripts/codex-worker.sh` for Codex coder + Claude reviewer. See `patterns.md` for all 6 tested patterns including Claude + Gemini mixed perspectives.
