# Multi-Agent Workflows

hcom is most useful when agents coordinate through explicit messages, event filters, transcripts, and bundles instead of ad-hoc copy/paste.

## Messaging Rules

Use intent to shape expected behavior:

```bash
hcom send @worker --intent request -- Implement the failing test.
hcom send @reviewer --intent inform -- Worker is ready for review.
hcom send @manager --intent ack --reply-to 42 -- Approved.
```

Use threads to isolate concurrent workflows:

```bash
thread="audit-$(date +%s)"
hcom send @planner @reviewer --thread "$thread" --intent request -- Start audit.
hcom send --thread "$thread" -- Next step for the same recipients.
```

Use tags for groups:

```bash
hcom 3 codex --tag audit --headless
hcom send @audit- --thread audit-1 -- Start independent review.
```

## Observation

Agents can inspect one another without requiring pasted context:

```bash
hcom transcript @worker --last 20
hcom transcript @worker 7-14 --full
hcom events --agent worker --last 20
hcom events --file '*.rs' --last 10
hcom term worker --json
```

## Subscriptions

Subscriptions turn future events into messages.

```bash
hcom events sub --idle worker
hcom events sub --blocked worker
hcom events sub --file '*.rs' --once
hcom events sub "type='message' AND msg_intent='request'"
```

Request messages create request-watch behavior so the sender can be notified when a requested agent stops or becomes idle.

Collision subscriptions watch concurrent file-write activity so two agents editing the same file can be warned quickly.

## Bundled Handoffs

Use bundles for structured handoffs:

```bash
hcom bundle prepare
hcom send @reviewer \
  --title "Implementation ready" \
  --description "Patch, events, and transcript range for review" \
  --events 120-135 \
  --files src/commands/send.rs,tests/cli_smoke.rs \
  --transcript 8-16:normal \
  --intent request \
  -- Please review the attached context.
```

The target can expand the bundle:

```bash
hcom bundle show <id>
hcom bundle cat <id>
```

## Common Topologies

| Pattern | Shape | Notes |
|---|---|---|
| Worker-reviewer | one worker, one reviewer | Worker sends completion, reviewer reads transcript/files/events and responds approve/fix. |
| Pipeline | planner -> implementer -> reviewer | Each stage waits for prior status/event, reads transcript or bundle, and sends the next message. |
| Ensemble | many workers, one judge | Workers answer independently on one thread; judge queries events/transcripts and synthesizes. |
| Hub-spoke | one coordinator, many workers | Coordinator launches/tag groups, broadcasts work, tracks results. |
| Reactive watcher | subscriptions and event filters | Agent reacts to idle, blocked, file write, command, or SQL matches. |

## Workflow Scripts

`hcom run` executes bundled and user scripts. User scripts live in:

```text
~/.hcom/scripts/
```

Rules for reliable scripts:

- Parse `--name`; hcom injects it when scripts are launched from an agent.
- Forward `--name` to hcom commands.
- Parse `Names: ...` from launch output and track launched agents.
- Use unique `--thread` values.
- Use `trap cleanup ERR INT TERM`.
- Use `hcom kill`, not only `stop`, for spawned headless workers.
- Use `--go` for unattended launch/kill operations.
- Use `hcom listen` or `hcom events --wait` instead of `sleep`.

Skeleton:

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
hcom events --wait 300 --thread "$thread" --intent ack >/dev/null
```
