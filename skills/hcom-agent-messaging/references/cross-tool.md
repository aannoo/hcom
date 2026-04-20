# Cross-Tool Patterns: Claude + Codex + Gemini + OpenCode

Verified behavior when mixing different AI coding tools via hcom.

## Why Mix Tools?

- **Claude Code**: Strong reasoning, planning, reviewing, and natural language. Full lifecycle hooks. Hook-based delivery.
- **Codex**: Runs in sandbox (workspace-write, untrusted, or full-access modes). Better for executing untrusted code, running tests, file manipulation. Full lifecycle hooks when hcom-launched.
- **Gemini CLI**: Strong reasoning with Google ecosystem integration. Full lifecycle hooks. Requires version >= 0.26.0.
- **OpenCode**: TypeScript plugin-based integration. TCP notify for instant wake. Plugin handler endpoints.

Typical combos: Claude designs/reviews + Codex implements, Claude plans + Gemini researches, multiple tools for diverse perspectives.

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

## Cross-Tool Gotchas

| Issue | Tool | Fix |
|-------|------|-----|
| Gemini hooks not working | Gemini | Requires version >= 0.26.0; check with `gemini --version` |
| OpenCode plugin not found | OpenCode | Run `hcom hooks add opencode` to install plugin |
| Cross-tool transcript reading | All | `hcom transcript @name --full --detailed` works across all tools |
| Different bootstrap formats | Mixed | Claude gets subagent section; Codex gets developer_instructions; Gemini gets system prompt file |
| Codex sandbox blocks hcom | Codex | Use `workspace` sandbox mode (default) which allows `~/.hcom` writes |
