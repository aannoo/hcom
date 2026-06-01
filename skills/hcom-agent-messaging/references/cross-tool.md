# Cross-Tool Patterns: Claude + Codex + Gemini + OpenCode + Kilo Code

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

## Working Patterns

See `scripts/cross-tool-duo.sh` for Claude architect + Codex engineer, and `scripts/codex-worker.sh` for Codex coder + Claude reviewer. See `patterns.md` for all 6 tested patterns including Claude + Gemini mixed perspectives.
