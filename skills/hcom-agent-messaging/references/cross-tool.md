# Cross-Tool Patterns: Claude + Codex + Gemini + OpenCode

Verified behavior when mixing different AI coding tools via hcom.

## Why Mix Tools?

- **Claude Code**: Best reasoning, planning, reviewing, and natural language. Hook-based delivery (<1s). 10 hooks for fine-grained lifecycle control.
- **Codex**: Runs in sandbox (workspace-write, untrusted, or full-access modes). Better for executing untrusted code, running tests, file manipulation. Single notify hook; PTY injection delivery.
- **Gemini CLI**: Strong reasoning with Google ecosystem integration. 7 hooks, hook-based delivery (<1s). Requires version >= 0.26.0.
- **OpenCode**: TypeScript plugin-based integration. TCP notify for instant wake. 4 handler endpoints.

Typical combos: Claude designs/reviews + Codex implements, Claude plans + Gemini researches, multiple tools for diverse perspectives.

## Per-Tool Technical Details

### Claude Code
- **Hooks**: 10 (SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, Stop, PermissionRequest, SubagentStart, SubagentStop, Notification, SessionEnd)
- **Payload**: JSON via stdin
- **Exit codes**: 0=allow, 2=block with message delivery
- **Session binding**: On SessionStart hook, immediate
- **Launch to ready**: 3-5 seconds
- **Message delivery**: Hook output in `additionalContext`, under 1s
- **Headless mode**: `-p` (print) flag for background, `setsid()` detach
- **Subagent support**: Yes, via Task with background=true
- **Bootstrap injection**: On SessionStart, includes full command reference, active agents, scripts

### Codex
- **Hooks**: 1 (`codex-notify` on agent-turn-complete)
- **Payload**: JSON via argv[2] (not stdin)
- **Session binding**: On first `codex-notify` event, 5-10 seconds after launch
- **Launch to ready**: 5-10 seconds
- **Message delivery**: PTY text injection, 1-3 seconds latency
- **Stale cleanup risk**: If session does not bind within 30 seconds, instance cleaned up as stale
- **Sandbox modes**: `workspace` (--full-auto + network), `untrusted` (--sandbox workspace-write), `danger-full-access` (--dangerously-bypass-approvals-and-sandbox), `none` (raw)
- **Bootstrap injection**: Via `-c developer_instructions=<bootstrap>` at launch time
- **Transcript path**: Derived from thread ID, searched via glob in `$CODEX_HOME/sessions/`

### Gemini CLI
- **Hooks**: 7 (sessionstart, beforeagent, afteragent, beforetool, aftertool, notification, sessionend)
- **Payload**: JSON via stdin
- **Session binding**: On beforeagent hook
- **Launch to ready**: 3-5 seconds
- **Message delivery**: Hook output, under 1s
- **System prompt**: Written to `~/.hcom/system-prompts/gemini.md`, set via `GEMINI_SYSTEM_MD` env var
- **Policy auto-approval**: `~/.gemini/policies/hcom.toml`
- **Transcript path**: Derived from session_id, searched in `~/.gemini/chats/`

### OpenCode
- **Hooks**: 4 (start, status, read, stop) via TypeScript plugin
- **Plugin location**: `$XDG_DATA_HOME/opencode/plugins/hcom/`
- **Session binding**: Via TCP binding ceremony (plugin calls `hcom opencode-start --session-id`)
- **Launch to ready**: 3-5 seconds
- **Message delivery**: Plugin TCP endpoint, under 1s
- **Auto-approval**: `OPENCODE_PERMISSION={"bash":{"hcom *":"allow"}}` env var

## Critical: Wait for Codex Before Sending Messages

Codex session binding is delayed. Without waiting, messages sent immediately after launch may never arrive:

```bash
# Launch Codex
launch_out=$(hcom 1 codex --tag eng --go --headless --hcom-prompt "..." 2>&1)
track_launch "$launch_out"
codex_name=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')

# MANDATORY: Wait for Codex session binding
hcom events --wait 30 --idle "$codex_name" $name_arg >/dev/null 2>&1
# Now safe to send messages to Codex
```

**Why this matters:**
1. Codex launches in ~5-10s but session only binds on first agent-turn-complete
2. If no activity happens within 30s, hcom cleanup marks instance as stale
3. Messages sent before binding have no recipient to deliver to
4. The `--idle` wait confirms the session has bound and agent is ready

## Working Pattern: Claude Architect + Codex Engineer

```bash
thread="duo-$(date +%s)"

# Claude designs spec
launch_out=$(hcom 1 claude --tag arch --go --headless \
  --hcom-prompt "Design spec: ${task}. Send to @eng-: hcom send \"@eng-\" --thread ${thread} --intent request -- \"SPEC: <spec>\". Wait for IMPLEMENTED. Send APPROVED. Stop." 2>&1)
track_launch "$launch_out"

# Codex implements
launch_out=$(hcom 1 codex --tag eng --go --headless \
  --hcom-prompt "Wait for spec from @arch-. Implement exactly as specified. Confirm: hcom send \"@arch-\" --thread ${thread} --intent inform -- \"IMPLEMENTED\". Stop." 2>&1)
track_launch "$launch_out"
eng_name=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')

# CRITICAL: wait for Codex
hcom events --wait 30 --idle "$eng_name" $name_arg >/dev/null 2>&1

# Wait for APPROVED
hcom events --wait 180 --sql "msg_thread='${thread}' AND msg_text LIKE '%APPROVED%'" $name_arg >/dev/null 2>&1
```

**Tested:** Claude sent "SPEC: Write a bash function named reverse_string..." -> Codex implemented -> Claude sent "APPROVED". ~30s total.

## Working Pattern: Codex Codes + Claude Reviews Transcript

```bash
# Codex does the coding
launch_out=$(hcom 1 codex --tag coder --go --headless \
  --hcom-prompt "Write and run: ${task}. Send output: hcom send \"@reviewer-\" --thread ${thread} --intent inform -- \"CODE DONE: <output>\". Stop." 2>&1)
track_launch "$launch_out"
coder_name=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')

# CRITICAL: wait for Codex
hcom events --wait 30 --idle "$coder_name" $name_arg >/dev/null 2>&1

# Claude reviews Codex's full transcript
launch_out=$(hcom 1 claude --tag reviewer --go --headless \
  --hcom-prompt "Wait for CODE DONE from @coder-. Read transcript: hcom transcript @${coder_name} --last 5 --full. Send REVIEWED: pass/fail. Stop." 2>&1)
track_launch "$launch_out"
```

**Tested:** Codex wrote `/tmp/calc.py`, output "2+2=4" -> Claude read transcript -> "REVIEWED: pass". ~35s total.

## Working Pattern: Claude + Gemini Mixed Perspectives

```bash
thread="mix-$(date +%s)"

# Claude analyzes from code perspective
launch_out=$(hcom 1 claude --tag code-review --go --headless \
  --hcom-prompt "Analyze ${task} from a code quality perspective. Send analysis: hcom send \"@judge-\" --thread ${thread} --intent inform -- \"CODE: <analysis>\". Stop." 2>&1)
track_launch "$launch_out"

# Gemini analyzes from architecture perspective
launch_out=$(hcom 1 gemini --tag arch-review --go --headless \
  --hcom-prompt "Analyze ${task} from an architecture perspective. Send: hcom send \"@judge-\" --thread ${thread} --intent inform -- \"ARCH: <analysis>\". Stop." 2>&1)
track_launch "$launch_out"

# Judge synthesizes
launch_out=$(hcom 1 claude --tag judge --go --headless \
  --hcom-prompt "Wait for CODE and ARCH analyses. Read: hcom events --sql \"msg_thread='${thread}'\" --last 10. Synthesize. Send VERDICT. Stop." 2>&1)
track_launch "$launch_out"
```

## Timing Data (Measured)

| Operation | Claude | Codex | Gemini | OpenCode |
|-----------|--------|-------|--------|----------|
| Launch to ready | 3-5s | 5-10s | 3-5s | 3-5s |
| Session binding | Instant | 5-10s | Instant | 2-3s |
| Message delivery | under 1s | 1-3s | under 1s | under 1s |
| Full 2-agent round-trip | 15-25s | 25-40s | 15-25s | 15-25s |
| Stale cleanup window | N/A | 30s | N/A | N/A |

## Cross-Tool Gotchas

| Issue | Tool | Fix |
|-------|------|-----|
| Codex cleaned up as stale | Codex | Wait for idle before messaging: `hcom events --wait 30 --idle $name` |
| Message never arrives to Codex | Codex | PTY injection requires idle state; ensure agent finished its turn |
| Gemini hooks not working | Gemini | Requires version >= 0.26.0; check with `gemini --version` |
| OpenCode plugin not found | OpenCode | Run `hcom hooks add opencode` to install plugin |
| Cross-tool transcript reading | All | `hcom transcript @name --full --detailed` works across all tools |
| Different bootstrap formats | Mixed | Claude gets subagent section; Codex gets developer_instructions; Gemini gets system prompt file |
| Codex sandbox blocks hcom | Codex | Use `workspace` sandbox mode (default) which allows `~/.hcom` writes |
