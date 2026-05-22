# Hook System

hcom hooks connect tool events to the hcom database and delivery layer. Hooks are optional but enable automatic status tracking and message delivery.

## Commands

```bash
hcom hooks
hcom hooks status
hcom hooks add [claude|gemini|codex|opencode|all]
hcom hooks remove [claude|gemini|codex|opencode|all]
```

Restart the relevant AI tool after adding hooks.

## Config Locations

When `HCOM_DIR` is unset, hooks target user-level tool config:

| Tool | Config path |
|---|---|
| Claude Code | `~/.claude/settings.json` |
| Codex | `~/.codex/config.toml` and `~/.codex/hooks.json` |
| Gemini CLI | `~/.gemini/settings.json` |
| OpenCode | hcom-managed plugin path |

When `HCOM_DIR` is set, hcom anchors tool config at the parent of `HCOM_DIR`. For example:

```bash
export HCOM_DIR="$PWD/.hcom"
hcom hooks add codex
```

This writes local tool config under the project parent, such as `$PWD/.codex/`, instead of the global home directory.

## Hook Gate

Hooks must be quiet when hcom is installed but not active. The shared hook gate proceeds when:

- the process is hcom-launched, or
- a hcom process binding exists, or
- the database already has instance rows.

If none of those are true, hooks exit without output.

## Session Binding

hcom resolves an event to an instance through:

1. `HCOM_PROCESS_ID` for hcom-launched tools.
2. tool session ID through `session_bindings`.
3. process binding through `process_bindings`.
4. bind markers discovered in transcripts for vanilla sessions that joined later.

Claude, Codex, Gemini, and OpenCode normalize their different hook payloads into a shared `HookPayload` model.

## Hook Events

Claude Code:

- `sessionstart`
- `userpromptsubmit`
- `pre`
- `post`
- `poll`
- `notify`
- `permission-request`
- `subagent-start`
- `subagent-stop`
- `sessionend`

Codex:

- `codex-sessionstart`
- `codex-userpromptsubmit`
- `codex-pretooluse`
- `codex-posttooluse`
- `codex-stop`

Gemini:

- `gemini-sessionstart`
- `gemini-aftertool`

OpenCode:

- `opencode-start`
- `opencode-status`
- `opencode-read`
- `opencode-stop`

## Delivery

Hooks deliver pending messages at safe points:

- session start binds or recovers identity.
- user prompt submission marks activity and can trigger delivery.
- pre-tool hooks record active tool status.
- post-tool hooks record results and deliver pending messages.
- stop/session-end hooks mark listening or stopped state.

Codex delivery uses hook `additionalContext` only, because using both system message and additional context creates duplicated visible output in Codex.

PTY delivery is separate and checks screen readiness plus `prompt_empty` before injecting text.

## Safe Auto-Approved Commands

When `auto_approve` is enabled, hcom can install permission rules for routine read/query/message commands. The source excludes destructive/admin actions such as `stop`, `kill`, `run`, and `reset`.

Safe commands include messaging, list/events/listen, relay/status/config queries, transcript, archive, bundle, term, hooks, and help/version commands.

## Claude Subagents

Claude subagent hooks track subagent lifecycle. hcom eagerly allocates an instance row when a subagent starts so it can be targeted by name. Dormant subagent rows are not fully announced until a message arrives or the subagent explicitly joins.

## Failure Behavior

Hook dispatch is panic-guarded. Hook errors are logged instead of crashing the host tool. Use:

```bash
hcom status --logs
```

to inspect recent hcom warnings and errors.
