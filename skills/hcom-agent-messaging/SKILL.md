---
name: hcom-agent-messaging
description: >
  Multi-agent communication with hcom. Use when agents need to message, watch,
  launch, fork, resume, script, or coordinate Claude Code, Codex, Gemini CLI,
  OpenCode, or ad-hoc tools across terminals or relay devices.
---

# hcom Agent Messaging

Use `hcom` when work spans multiple AI sessions, terminals, or devices. hcom gives agents names, message delivery, event history, transcripts, bundles, subscriptions, workflow scripts, and launch/fork/resume controls.

## First Checks

```bash
hcom status
hcom list
hcom --help
```

If `hcom` is unavailable, install it:

```bash
brew install aannoo/hcom/hcom
# or
curl -fsSL https://github.com/aannoo/hcom/releases/latest/download/hcom-installer.sh | sh
```

If hooks are missing:

```bash
hcom hooks add all
hcom hooks status
```

Restart the affected AI tool after adding hooks.

Inside a sandbox that cannot write to `~/.hcom`:

```bash
export HCOM_DIR="$PWD/.hcom"
hcom start
```

## Public DeepWiki Boundary

When using DeepWiki to understand public `aannoo/hcom`, use hosted public DeepWiki MCP (`mcp__deepwiki__`, `https://mcp.deepwiki.com/mcp`), not DeepWiki Local.

Use DeepWiki Local (`mcp__deepwiki_local__`) only for private/local filesystem analysis or explicit local DeepWiki tasks.

## Core Model

- Instance: named hcom participant.
- Session: tool session bound to an instance.
- Event: immutable message/status/lifecycle record.
- Message: event with sender, text, mentions, intent, thread, reply, and optional bundle.
- Bundle: structured references to events, files, and transcript ranges.
- Relay peer: remote trusted device, shown as `name:DEVICE`.

## Common Commands

| Task | Command |
|---|---|
| Open dashboard | `hcom` |
| Launch agents | `hcom claude`, `hcom 2 codex`, `hcom 1 gemini --tag audit --headless` |
| Join manually | `hcom start`, `hcom start --as <name>` |
| List agents | `hcom list`, `hcom list --json`, `hcom list --names` |
| Message | `hcom send @name -- text` |
| Wait | `hcom listen`, `hcom listen 30`, `hcom listen --idle name` |
| Query events | `hcom events --last 20`, `hcom events --agent name` |
| Subscribe | `hcom events sub --idle name`, `hcom events sub --file '*.py' --once` |
| Read transcript | `hcom transcript name --last 20`, `hcom transcript name 7-12 --full` |
| View terminal | `hcom term name --json` |
| Inject terminal text | `hcom term inject name "text" --enter` |
| Prepare bundle | `hcom bundle prepare` |
| Expand bundle | `hcom bundle show <id>`, `hcom bundle cat <id>` |
| Resume/fork | `hcom r name`, `hcom f name` |
| Kill | `hcom kill name`, `hcom kill tag:tag`, `hcom kill all` |
| Relay | `hcom relay new`, `hcom relay connect <token>`, `hcom relay status` |
| Scripts | `hcom run`, `hcom run docs --scripts`, `hcom run <script>` |

## Messaging

```bash
hcom send @luna -- Hello
hcom send @luna @nova --intent request -- Can you review this?
hcom send @audit- --thread audit-1 -- Start independent review
hcom send @luna --reply-to 42 --intent ack -- Fixed
hcom send @luna:BOXE -- Remote message
```

Target rules:

- `@luna`: base name; can match tag-prefixed names.
- `@audit-`: all with tag prefix.
- `@audit-luna`: exact full display name.
- `@luna:BOXE`: remote relay target.
- underscore blocks prefix matching.

Intent rules:

- `request`: always respond.
- `inform`: respond only if useful.
- `ack`: do not respond; must include `--reply-to`.

Thread rule:

- Seed a thread with recipients once, then use `--thread <name>` to keep later messages scoped.

## Observing Other Agents

Use hcom evidence instead of asking agents to paste context:

```bash
hcom transcript @worker --last 20
hcom transcript @worker 8-14 --full
hcom events --agent worker --last 20
hcom events --file '*.rs' --last 10
hcom term worker --json
```

## Bundles

Create handoffs without dumping large content into the message:

```bash
hcom send @reviewer \
  --title "Review worker result" \
  --description "Files, event range, and transcript for review" \
  --events 120-135 \
  --files src/main.rs,tests/cli_smoke.rs \
  --transcript 8-14:normal \
  --intent request \
  -- Please review this bundle.
```

Recipients can inspect:

```bash
hcom bundle show <id>
hcom bundle cat <id>
hcom bundle chain <id>
```

Transcript detail levels: `normal`, `full`, `detailed`.

## Workflow Patterns

| Pattern | Shape | Use |
|---|---|---|
| Worker-reviewer | one worker, one reviewer | implementation plus review loop |
| Pipeline | planner -> executor -> reviewer | sequential staged work |
| Ensemble | many workers, one judge | independent opinions plus synthesis |
| Hub-spoke | coordinator plus workers | split work, collect reports |
| Reactive | subscriptions plus handlers | act on idle/blocked/file/command events |

## Script Rules

Read `references/script-template.md` before writing a new script. Existing examples are in `references/scripts/`.

Mandatory script practices:

- Parse and forward `--name`.
- Capture launched names from `Names: ...`.
- Use unique `--thread` values.
- Wait for `active` or `listening` before sending work.
- Use `trap cleanup ERR INT TERM`.
- Use `hcom kill`, not only `stop`, for spawned workers.
- Use `--go` in unattended launch/kill commands.
- Use `hcom listen` or `hcom events --wait`; do not use arbitrary `sleep`.

Minimal shell skeleton:

```bash
#!/usr/bin/env bash
set -euo pipefail

name_flag=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --name) name_flag="$2"; shift 2 ;;
    *) shift ;;
  esac
done

name_arg=()
[[ -n "$name_flag" ]] && name_arg=(--name "$name_flag")

thread="workflow-$(date +%s)"
launched=()

cleanup() {
  for name in "${launched[@]}"; do
    hcom kill "$name" --go >/dev/null 2>&1 || true
  done
}
trap cleanup ERR INT TERM

out=$(hcom 1 codex --tag worker --headless --go 2>&1)
names=$(printf '%s\n' "$out" | grep '^Names: ' | sed 's/^Names: //')
for n in $names; do launched+=("$n"); done

hcom send @worker- "${name_arg[@]}" --thread "$thread" --intent request -- "Do the task"
```

## Relay Rules

Relay is for one trusted operator's devices.

```bash
hcom relay new
hcom relay connect <token>
hcom relay status
```

Security notes:

- Payloads use XChaCha20-Poly1305.
- Token includes relay ID, broker URL, and raw PSK.
- Relay membership is full trust.
- No per-device read-only roles.
- Leaked PSK means create a new relay group.

## Troubleshooting

| Symptom | Check |
|---|---|
| command missing | `hcom status`, install hcom |
| hooks inactive | `hcom hooks status`, then `hcom hooks add <tool>` and restart |
| target not found | `hcom list`, use exact full name or remote suffix |
| message not delivered | `hcom events --last 20`, check mentions/thread/intent |
| sandbox write failure | set `HCOM_DIR="$PWD/.hcom"` |
| stale runtime state | inspect `hcom archive` before `hcom reset` |
| relay confusion | `hcom relay status`, check device suffix and token trust |

## Reference Files

| File | Use |
|---|---|
| `references/patterns.md` | workflow topologies and examples |
| `references/cross-tool.md` | Claude, Codex, Gemini, OpenCode quirks |
| `references/gotchas.md` | timing, delivery, cleanup, intent issues |
| `references/script-template.md` | annotated script template |
| `references/scripts/` | tested example scripts |

Project docs in this fork:

- `docs/architecture.md`
- `docs/command-reference.md`
- `docs/configuration.md`
- `docs/hook-system.md`
- `docs/multi-agent-workflows.md`
- `docs/relay.md`
- `docs/development.md`
