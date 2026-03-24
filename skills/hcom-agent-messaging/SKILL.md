---
name: hcom-agent-messaging
description: |
  Let AI agents message, watch, and spawn each other across terminals. Claude Code, Gemini CLI, Codex, OpenCode. Use this skill when the human user needs help, status, or reference about hcom - when user asks questions like "how to setup hcom", "hcom not working", "explain hcom", or any hcom troubleshooting.

---

# hcom — multi-agent communication for AI coding tools

AI agents running in separate terminals are isolated. hcom connects them via hooks and a shared database so they can message, watch, and spawn each other in real-time.

```bash
curl -fsSL https://raw.githubusercontent.com/aannoo/hcom/main/install.sh | sh
hcom claude       # or: hcom gemini, hcom codex, hcom opencode
hcom              # TUI dashboard
```

---

## what humans can do

tell any agent:

> send a message to claude

> when codex goes idle send it the next task

> watch gemini's file edits, review each and send feedback if any bugs

> fork yourself to investigate the bug and report back

> find which agent worked on terminal_id code, resume them and ask why it sucks

---

## what agents can do

| capability | command | tested behavior |
|------------|---------|-----------------|
| message each other | `hcom send @name -- "msg"` | delivers in under 1s (claude), 1-3s (codex) |
| read each other's transcripts | `hcom transcript @name --full` | includes tool I/O with `--detailed` |
| view/inject into terminal screens | `hcom term inject name "text" --enter` | approves prompts, types commands |
| query event history | `hcom events --sql "..." --last N` | file edits, commands, status, lifecycle |
| subscribe to activity | `hcom events sub --idle name` | real-time notifications via system messages |
| spawn/fork/resume/kill agents | `hcom 1 claude --tag X --go --headless` | headless background agents for scripts |
| build context bundles | `hcom send --title X --transcript 1-20:full --files a.py` | structured handoffs between agents |
| collision detection | automatic | 2 agents edit same file within 30s, both notified |
| cross-device messaging | `hcom send @name:DEVICE -- "msg"` | via MQTT relay, 1-5s latency |

---

## essential commands (quick reference)

### launch agents
```bash
hcom 1 claude --tag worker --go --headless --hcom-prompt "your task"
hcom 1 codex --tag engineer --go --headless --hcom-prompt "your task"
hcom 3 claude --tag team --go --headless    # batch launch
```

flags: `--go` (skip confirmation), `--headless` (background), `--tag X` (group routing), `--hcom-prompt` (initial task), `--hcom-system-prompt` (role/personality)

### send messages
```bash
hcom send @luna -- "direct message"
hcom send @worker- -- "all agents tagged worker"
hcom send @luna @nova -- "multiple targets"
hcom send -- "broadcast to everyone"
hcom send @luna --intent request -- "expecting reply"
hcom send @luna --intent inform -- "fyi, no reply needed"
hcom send @luna --thread review-1 -- "threaded conversation"
```

### wait for events (replaces sleep in scripts)
```bash
hcom events --wait 120 --sql "msg_text LIKE '%DONE%'"
hcom events --wait 60 --idle luna
hcom listen 30    # block until any message
```

### read other agents' work
```bash
hcom transcript @luna --full              # complete conversation
hcom transcript @luna --full --detailed   # with tool I/O, file edits
hcom transcript @luna --last 3            # last 3 exchanges
```

### agent lifecycle
```bash
hcom list                # show all active agents
hcom kill luna --go      # kill + close terminal pane
hcom kill all --go       # kill everything
hcom stop luna --go      # graceful stop (preserves session)
hcom r luna              # resume stopped agent
hcom f luna --tag rev    # fork (clone session, new identity)
```

### event subscriptions (reactive)
```bash
hcom events sub --idle luna              # notify when luna goes idle
hcom events sub --file "*.py" --once     # one-shot: next .py file edit
hcom events sub --collision              # file edit collisions
```

---

## tool support

| tool | hooks | delivery | session binding | launch to ready |
|------|-------|----------|-----------------|-----------------|
| claude code (incl. subagents) | 10 hooks | automatic (hook output) | instant on sessionstart | 3-5s |
| gemini cli (>= 0.26.0) | 7 hooks | automatic (hook output) | instant on beforeagent | 3-5s |
| codex | 1 hook (notify) | PTY injection | on first agent-turn-complete | 5-10s |
| opencode | 4 handlers (plugin) | plugin TCP endpoint | on plugin binding | 3-5s |
| any ai tool | manual | `hcom start` inside tool | on first command | varies |

**codex note:** session binding takes 5-10s. always wait before sending messages:
```bash
hcom events --wait 30 --idle "$codex_name"
```

---

## setup

if the user invokes this skill without arguments:

1. run `hcom status` — if "command not found", install first:
   ```bash
   curl -fsSL https://raw.githubusercontent.com/aannoo/hcom/main/install.sh | sh
   ```
2. run `hcom hooks add` to install hooks for all detected tools
3. restart the AI tool for hooks to activate

| status output | meaning | action |
|---------------|---------|--------|
| command not found | not installed | install via curl or `pip install hcom` |
| `[~] claude` | tool exists, hooks not installed | `hcom hooks add` then restart |
| `[✓] claude` | hooks installed | ready |
| `[✗] claude` | tool not found | install the AI tool first |

---

## troubleshooting

### "hcom not working"

```bash
hcom status          # check installation
hcom hooks status    # check hooks specifically
hcom relay status    # check cross-device relay
```

hooks missing? `hcom hooks add` then restart tool.

still broken?
```bash
hcom reset all && hcom hooks add
# close all ai tool windows
hcom claude          # fresh start
```

### "messages not arriving"

| symptom | diagnosis | fix |
|---------|-----------|-----|
| agent not in `hcom list` | agent stopped or never bound | relaunch or wait for binding |
| message sent but not delivered | check `hcom events --last 5` | verify @mention matches agent name/tag |
| codex agent shows inactive | stale_cleanup (session didn't bind in 30s) | wait for idle before sending |
| wrong agent receives message | @mention ambiguity | use `@tag-` prefix for reliable routing |
| messages leaking between workflows | no thread isolation | always use `--thread` |

### intent system

agents follow these rules from their bootstrap:
- `--intent request` → agent always responds
- `--intent inform` → agent responds only if useful
- `--intent ack` → agent does not respond

### sandbox / permission issues

```bash
export HCOM_DIR="$PWD/.hcom"     # project-local mode
hcom hooks add                   # installs to project dir
```

---

## scripting patterns (tested with real agents)

### basic two-agent messaging
```bash
thread="t-$(date +%s)"
launch_out=$(hcom 1 claude --tag worker --go --headless \
  --hcom-prompt "count 1-5. send result: hcom send \"@reviewer-\" --thread ${thread} --intent inform -- \"RESULT: <answer>\". stop: hcom stop" 2>&1)
worker=$(echo "$launch_out" | grep '^Names: ' | sed 's/^Names: //' | tr -d ' ')

launch_out=$(hcom 1 claude --tag reviewer --go --headless \
  --hcom-prompt "wait for @worker-. send DONE to bigboss. stop: hcom stop" 2>&1)

hcom events --wait 120 --sql "msg_thread='${thread}' AND msg_text LIKE '%DONE%'"
hcom kill all --go
```

### worker-reviewer feedback loop
```bash
# worker does task, reviewer evaluates, sends APPROVED or FIX feedback
# worker corrects if needed, loop until approved
# see references/patterns.md for full tested scripts
```

### cross-tool: claude designs, codex implements
```bash
# claude sends spec to @eng- (codex), codex implements, claude approves
# CRITICAL: wait for codex binding before messaging
hcom events --wait 30 --idle "$codex_name"
# see references/patterns.md for full tested scripts
```

### ensemble: 3 agents + judge
```bash
# 3 agents independently answer, judge aggregates via hcom events --sql
# agents run in parallel (same wall-clock as 1 agent)
# see references/patterns.md for full tested scripts
```

---

## script template (for workflow scripts in ~/.hcom/scripts/)

```bash
#!/usr/bin/env bash
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
# your logic here
```

key rules:
- always parse and forward `--name` (hcom injects it)
- always use `--go` on launch/kill
- always use `--headless` for script agents
- always use `--thread` for message isolation
- use `hcom events --wait` instead of `sleep`
- capture agent names from `grep '^Names: '` in launch output
- cleanup with `trap cleanup ERR` + `hcom kill`

---

## files

| what | location |
|------|----------|
| database | `~/.hcom/hcom.db` |
| config | `~/.hcom/config.toml` |
| logs | `~/.hcom/.tmp/logs/` |
| user scripts | `~/.hcom/scripts/` |

with `HCOM_DIR` set, uses that path instead of `~/.hcom`.

---

## reference files

| file | when to read |
|------|-------------|
| `references/patterns.md` | writing multi-agent scripts — 6 tested patterns with full code and real event JSON |
| `references/cli-reference.md` | complete command reference with all flags and examples |
| `references/cross-tool.md` | claude + codex + gemini + opencode collaboration details and per-tool quirks |
| `references/gotchas.md` | debugging scripts — timing, stale agents, codex binding, message delivery issues |

---

## more info

```bash
hcom --help              # all commands
hcom <command> --help    # command details
hcom run docs            # full CLI + config + API reference
```

github: https://github.com/aannoo/hcom
