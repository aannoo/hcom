---
name: hcom-agent-messaging
description: >
  Multi-agent communication for AI coding tools. Agents message, watch,
  and spawn each other across terminals. Use when setting up hcom,
  troubleshooting delivery, or writing multi-agent scripts.
---

# hcom — multi-agent communication for AI coding tools

AI agents running in separate terminals are isolated. hcom connects them via hooks and a shared database so they can message, watch, and spawn each other in real-time.

```bash
curl -fsSL https://github.com/Solar2004/hcom/releases/latest/download/hcom-installer.sh | sh
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

**Message** each other in real-time, bundle context for handoffs.

**Observe** each other: transcripts, file edits, terminal screens, command history.

**Subscribe** to each other: notify on status changes, file edits, specific events. React automatically.

**Spawn**, **fork**, **resume**, **kill** each other, in any terminal emulator.

run `hcom --help` for full command syntax and flags.

---

## tool support

| tool | delivery | connect |
|------|----------|---------|
| claude code (incl. subagents) | automatic | `hcom claude` |
| gemini cli (>= 0.26.0) | automatic | `hcom gemini` |
| codex | automatic | `hcom codex` |
| opencode | automatic | `hcom opencode` |
| any other ai tool | manual via `hcom listen` | `hcom start` (run inside tool) |

session binding (hcom transcript, hcom r/f by session id) happens on first message or first prompt for all hcom-launched tools.

---

## setup

if the user invokes this skill without arguments:

1. run `hcom status` — if "command not found", install first:
   ```bash
   curl -fsSL https://github.com/Solar2004/hcom/releases/latest/download/hcom-installer.sh | sh
   ```
2. run `hcom hooks add` to install hooks for all detected tools
3. restart the AI tool for hooks to activate

| status output | meaning | action |
|---------------|---------|--------|
| command not found | not installed | install via `brew install Solar2004/hcom/hcom`, the curl installer above, or `pip install hcom` |
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
| wrong agent receives message | @mention ambiguity | use `@tag-` prefix for reliable routing |
| messages leaking between workflows | no thread isolation | always use `--thread` |

### intent system

agents follow these rules from their bootstrap:
- `--intent request` -> agent always responds
- `--intent inform` -> agent responds only if useful
- `--intent ack` -> agent does not respond

### sandbox / permission issues

```bash
export HCOM_DIR="$PWD/.hcom"     # project-local mode
hcom hooks add                   # installs to project dir
```

---

## workflow scripting

place scripts in `~/.hcom/scripts/` as `.sh` or `.py`. run with `hcom run <name> "task"`. see `references/script-template.md` for the full annotated template, or run `hcom run docs --scripts` inside an agent.

### key rules

- **never use `sleep`** — use `hcom events --wait` or `hcom listen`
- **never hardcode agent names** — parse from `grep '^Names: '` in launch output
- **always use `--thread`** — without it, messages leak across workflows
- **always use `trap cleanup ERR INT TERM`** — orphan headless agents run indefinitely
- **always use `hcom kill` for cleanup** (not `stop`) — kill also closes the terminal pane
- **always forward `--name`** — hcom injects it, scripts must propagate it
- **always use `--go`** on launch/kill — without it, scripts hang on confirmation prompt

### agent topologies

| topology | agents | pattern |
|----------|--------|---------|
| worker-reviewer | 2 | worker sends result, reviewer reads transcript, sends APPROVED/FIX |
| pipeline | N sequential | each stage reads previous via `hcom transcript`, signals via thread |
| ensemble | N+1 (judge) | N agents answer independently, judge reads all via `hcom events --sql` |
| hub-spoke | 1+N | coordinator broadcasts to `@tag-`, workers report back |
| reactive | N | `hcom events sub` triggers agent actions on file edits/status changes |

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
| `references/cross-tool.md` | claude + codex + gemini + opencode collaboration details and per-tool quirks |
| `references/gotchas.md` | debugging scripts — timing, message delivery, intent system, cleanup |
| `references/script-template.md` | writing a new script from scratch — full template with commentary |
| `references/scripts/` | 6 tested, working example scripts |

---

## more info

```bash
hcom --help              # all commands
hcom <command> --help    # command details
```

github: https://github.com/Solar2004/hcom
