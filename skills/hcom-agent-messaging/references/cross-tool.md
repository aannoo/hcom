# Cross-Tool Patterns: Claude + Codex + Gemini + OpenCode

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
- **Plugin location**: `$XDG_DATA_HOME/opencode/plugins/hcom/`
- **Session binding**: Via TCP binding ceremony (plugin calls `hcom opencode-start --session-id`)
- **Message delivery**: Plugin TCP endpoint
- **Auto-approval**: `OPENCODE_PERMISSION={"bash":{"hcom *":"allow"}}` env var

## Working Patterns

See `scripts/cross-tool-duo.sh` for Claude architect + Codex engineer, and `scripts/codex-worker.sh` for Codex coder + Claude reviewer. See `patterns.md` for all 6 tested patterns including Claude + Gemini mixed perspectives.
