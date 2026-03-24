---
name: hcom-workflow-scripts
description: |
  Build multi-agent workflow scripts using hcom. Use this skill when the user wants to create custom hcom scripts, design multi-agent pipelines, write automation that coordinates Claude and Codex agents, or build applications that use hcom as the communication backbone. Covers script patterns, agent topologies, hcom internals, and tested examples.

---

# hcom workflow scripts

build custom multi-agent workflow scripts that launch, coordinate, and manage AI coding agents via hcom.

## decision tree

1. **write a new script** → use the script template below + read `references/script-template.md`
2. **choose a multi-agent pattern** → read `references/patterns.md`
3. **need hcom architecture details** → read `references/architecture.md`
4. **cross-tool scripting (claude + codex)** → read `references/cross-tool-scripting.md`
5. **debug a script** → read `references/debugging.md`
6. **want pre-built scripts** → check `references/scripts/` directory

## script template

every hcom workflow script follows this structure:

```bash
#!/usr/bin/env bash
# description shown in `hcom run` listing.
set -euo pipefail

LAUNCHED_NAMES=()
track_launch() {
  local names=$(echo "$1" | grep '^Names: ' | sed 's/^Names: //')
  for n in $names; do LAUNCHED_NAMES+=("$n"); done
}
cleanup() {
  for name in "${LAUNCHED_NAMES[@]}"; do
    hcom kill "$name" --go 2>/dev/null || true
  done
}
trap cleanup ERR

name_flag=""
task=""
while [[ $# -gt 0 ]]; do
  case "$1" in --name) name_flag="$2"; shift 2 ;; -*) shift ;; *) task="$1"; shift ;; esac
done
name_arg=""
[[ -n "$name_flag" ]] && name_arg="--name $name_flag"

thread="workflow-$(date +%s)"

# --- your workflow logic ---

# launch agent
launch_out=$(hcom 1 claude --tag worker --go --headless \
  --hcom-prompt "task: ${task}. when done: hcom send \"@reviewer-\" --thread ${thread} --intent inform -- \"DONE: <result>\". then: hcom stop" 2>&1)
track_launch "$launch_out"
worker=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')

# wait for signal
hcom events --wait 120 --sql "type='message' AND msg_thread='${thread}' AND msg_text LIKE '%DONE%'" $name_arg >/dev/null 2>&1

# cleanup
trap - ERR
for name in "${LAUNCHED_NAMES[@]}"; do hcom kill "$name" --go 2>/dev/null || true; done
```

place scripts in `~/.hcom/scripts/` as `.sh` or `.py`. run with `hcom run <name> "task"`.

## agent topologies

| topology | agents | hcom primitives |
|----------|--------|-----------------|
| worker-reviewer | 2 | worker sends result, reviewer reads transcript, sends APPROVED/FIX |
| pipeline | N sequential | each stage reads previous via `hcom transcript`, signals via thread |
| ensemble | N+1 (judge) | N agents answer independently, judge reads all via `hcom events --sql` |
| hub-spoke | 1+N | coordinator broadcasts to `@tag-`, workers report back |
| reactive | N | `hcom events sub` triggers agent actions on file edits/status changes |

## communication primitives

| what | how | latency |
|------|-----|---------|
| send message | `hcom send @name --thread T --intent X -- "msg"` | under 1s (claude), 1-3s (codex) |
| wait for signal | `hcom events --wait N --sql "..."` | under 1s after match |
| read agent's work | `hcom transcript @name --full --detailed` | under 1s |
| react to file changes | `hcom events sub --file "*.py"` | under 2s |
| react to agent idle | `hcom events sub --idle name` | under 2s |
| cross-device | `hcom send @name:DEVICE -- "msg"` | 1-5s |
| structured handoff | `hcom send --title X --transcript N-M:full --files a.py` | under 1s |

## required flags for script launches

| flag | why it's required |
|------|-------------------|
| `--go` | skips confirmation prompt — without it, script hangs forever |
| `--headless` | runs agent as detached background process |
| `--tag X` | groups agents for `@X-` routing (essential for scripts) |
| `--hcom-prompt "..."` | sets the agent's initial task |

## key rules

- **never use `sleep`** — use `hcom events --wait` or `hcom listen`
- **never hardcode agent names** — parse from `grep '^Names: '` in launch output
- **always use `--thread`** — without it, messages leak across workflows
- **always use `trap cleanup ERR`** — orphan headless agents run indefinitely
- **always use `hcom kill` for cleanup** (not `stop`) — kill also closes the terminal pane
- **always forward `--name`** — hcom injects it, scripts must propagate it
- **wait for codex before messaging** — `hcom events --wait 30 --idle "$codex_name"`

## timing reference (measured)

| operation | claude | codex |
|-----------|--------|-------|
| launch to ready | 3-5s | 5-10s |
| message delivery | under 1s | 1-3s |
| transcript read | under 1s | under 1s |
| full 2-agent round-trip | 15-25s | 25-40s |
| full 4-agent ensemble | 25-35s | n/a |

## integration with external systems

hcom scripts are bash — they can call any CLI tool:

```bash
# ci/cd trigger
hcom send @worker- --from "github-actions" -- "PR merged, deploy"

# webhook notification
curl -X POST $WEBHOOK -d "$(hcom events --sql "msg_thread='${thread}'" --last 5)"

# pipe output
echo "complex task description" | hcom send @worker-
```

## reference files

| file | when to read |
|------|-------------|
| `references/script-template.md` | writing a new script from scratch — full template with commentary |
| `references/patterns.md` | choosing and implementing multi-agent patterns — 6 tested examples |
| `references/architecture.md` | understanding hcom internals — db schema, hooks, delivery pipeline, events |
| `references/cross-tool-scripting.md` | claude + codex mixed scripts — per-tool quirks, timing, session binding |
| `references/debugging.md` | fixing broken scripts — common failures, stale agents, message delivery |
| `references/scripts/basic-messaging.sh` | tested: two agents exchange messages |
| `references/scripts/review-loop.sh` | tested: worker-reviewer feedback loop |
| `references/scripts/cross-tool-duo.sh` | tested: claude architect + codex engineer |
| `references/scripts/ensemble-consensus.sh` | tested: 3 independent agents + judge |
| `references/scripts/cascade-pipeline.sh` | tested: sequential plan then execute pipeline |
| `references/scripts/codex-worker.sh` | tested: codex codes, claude reviews transcript |
