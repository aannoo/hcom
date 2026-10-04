---
name: hcom-agent-messaging
description: >
  Multi-agent communication for AI coding tools. Agents message, watch,
  and spawn each other across terminals. Use when setting up hcom or
  troubleshooting.
---

# hcom — multi-agent communication for AI coding tools

AI agents running in separate terminals are isolated. hcom connects them via hooks and a shared database so they can message, watch, and spawn each other in real-time.

Start an agent with `hcom` in front, then prompt normally.

Use hcom to:

- coordinate multi-agent pipelines
- run different AI CLIs as each other's subagents
- avoid copy-pasting

```bash
uv tool install hcom
hcom claude       # or: hcom gemini, hcom codex, hcom opencode, hcom kilo, hcom pi, hcom omp, hcom agy, hcom cursor, hcom kimi, hcom copilot, hcom qoder, hcom grok
hcom              # TUI dashboard
```

Quickstart:

```bash
# terminal 1
hcom claude

# terminal 2
hcom codex
```

Prompt normally — e.g. `review what claude did and send it fixes`

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

**Message** each other in real time: mid-turn or wake immediately when idle

**Observe** each other: status, transcripts, file edits, live terminal screens, command history.

**Subscribe** and notify on status changes, file edits, collisions, specific events. React automatically.

**Spawn**, **fork**, **resume**, **kill** in any terminal emulator or headless.

run `hcom --help` for full command syntax and flags.

---

Works with Claude Code, Gemini CLI, Codex, OpenCode, Kilo Code, Pi, Oh My Pi, Antigravity, Cursor, Kimi, Copilot, Qoder CLI, Grok Build, and other tools

---

## setup

When this skill is invoked, first run:

```bash
hcom status
```

If hcom status works: run `hcom list`

If hcom list shows "Your name: <name>" where `<name>` is not "(not participating)": congratulations!

If running `hcom status` returns "command not found", install first:
```bash
uv tool install hcom
```

See the [hcom README](https://github.com/aannoo/hcom#install) for other install options.

If hcom status shows a list of CLI tools and you are not any of them, run `hcom start` to connect to hcom.

If you've just installed hcom now or if hcom list shows "Your name: (not participating)" and you are a tool in the hcom status list of CLI tools:

Relaunch into hcom properly for automatic message delivery and full hcom functionality. User should exit and run `hcom <tool>`.
Or if you know your sessionID/ses_/thread_name: user can exit and run `hcom r <session-id>` to resume you inside hcom. See `hcom r --help`.

---

## troubleshooting

### "hcom not working"

```bash
hcom status          # check installation
hcom hooks status    # check hooks specifically
hcom relay status    # check cross-device relay
```

still broken?
```bash
hcom reset all # backup config/db + reset it
hcom claude          # fresh start
```

still broken after that?
```bash
git clone https://github.com/aannoo/hcom.git
cd hcom
```
Read code and figure out what is going on.


### sandbox / permission issues

Use project-local hcom state when the normal hcom directory is unavailable:

```bash
HCOM_DIR=$PWD/.hcom hcom <tool>
```

## files

| what | location |
|------|----------|
| database | `~/.hcom/hcom.db` |
| config | `~/.hcom/config.toml` |
| env | `~/.hcom/env` |
| logs | `~/.hcom/.tmp/logs/` |
| user scripts | `~/.hcom/scripts/` |

---

## more info

```bash
hcom --help
hcom <command> --help
hcom run docs --scripts   # script authoring info
```

Github: https://github.com/aannoo/hcom
