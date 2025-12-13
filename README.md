# hcom - Claude Hook Comms

[![PyPI - Version](https://img.shields.io/pypi/v/hcom)](https://pypi.org/project/hcom/)
[![PyPI - License](https://img.shields.io/pypi/l/hcom)](https://opensource.org/license/MIT) [![Python Version](https://img.shields.io/badge/python-3.10+-blue.svg)](https://python.org) [![DeepWiki](https://img.shields.io/badge/DeepWiki-docs-blue.svg)](https://deepwiki.com/aannoo/claude-hook-comms)

Real-time communication layer for Claude Code via hooks.

![Demo](https://raw.githubusercontent.com/aannoo/claude-hook-comms/main/screencapture.gif)

## Install

```bash
pip install hcom && hcom
```


## What it does

```
                              ┌───────────┐
┌──────────┐ hcom send 'hi'   │ Claude B  │──► wakes instantly:
│ Claude A │────────┬────────►│ (idle)    │    [new message] 'hi'
│   and    │        │         └───────────┘
│ friends* │        │         ┌───────────┐
└──────────┘        └────────►│ Claude C  │──► after current tool:
                              │ (working) │    [new message] 'hi'
                              └───────────┘
```
> ***Friends** == interactive claude terminals, headless (-p) instances, task tool subagents, the TUI, any external process. not real friends.

- Any Claude can join (`hcom start`) or leave (`hcom stop`) at runtime.
- Normal `claude` sessions are unaffected until you opt in.
- Works on Mac, Linux, Windows/WSL, Android.


**What gets installed:**
- `~/.hcom/` — database, config, logs
- `~/.claude/settings.json` — hooks

Safely remove with `hcom reset all`


## How It Works

When Claude finishes doing some work, the stop hook runs and Claude asks it "Can I stop now?" expecting a quick yes/no. hcom never answers. It waits, sitting in select(), until a message shows up. Then it wakes up and says:

"NO, you can't stop, because john says hi"

`{"decision": "block", "reason": "[new message] john -> you: hi"}`

Claude reads the "reason" it can't stop - which is just the message - and keeps going.

Hooks also log all activity as events Claude can subscribe to. Example:

- ClaudeA edits `hi.txt` → hook → logged
- ClaudeB edits `hi.txt` 5s later → hook → collision detected → both get notified


## Features

### "Instant" Messaging
```bash
hcom send "hello everyone"          # broadcast
hcom send "@john check this"        # direct message

HCOM_TAG=backend hcom 2             # Group: 2 backend- instances
HCOM_TAG=frontend hcom 2            # Group: 2 frontend- instances
hcom send "@backend scale up"       # Message entire backend group
```

### Subagent Communication
Task tool subagents get their own identities and can talk to each other:
```
Parent: alice
  ├── alice_reviewer_1  ──┐
  ├── alice_explorer_1  ──┼── can message each other AND parent
  └── alice_planner_1   ──┘
```
Subagents stay alive after they finish their task (configurable timeout).
Parents can communicate with subagents when they're in the background.

**Example:** 

`"Use background task subagents to compare 3 different codebases, have each use hcom to send updates and you send follow-ups via hcom to steer and cross‑question them, then synthesize a final comparison"`

### External Tools
Any process can join the conversation:
```bash
hcom send --from ci "build passed"              # one-shot
hcom send --from gemini "analysis done" --wait  # block until reply
```
External senders appear in the TUI like any other instance

### Cross-Device Sync

Send and receive messages between machines via a lightweight HTTP relay.

**Setup with HuggingFace Spaces:**
```bash
hcom relay hf <token>   # get write token from huggingface.co/settings/tokens
hcom relay hf           # or use existing huggingface-cli login
```
Creates a free, private Space on your HuggingFace account.

```bash
hcom relay              # check if it's working
```

### Events

Everything is logged to SQLite. Query and subscribe.

**View events:**
```bash
hcom events                                   # recent events
hcom events --sql "instance='john'"           # filter with SQL
```

**"Push" notification subscriptions:**

When a matching event occurs, claude receives a system message with the event data.

```bash
# Instances are notified when another instance edits the same file within ~20s
hcom events sub collision
```

Prompt claude: `"Detect when john finished and spam them with more work"`
```bash
# claude creates subscription
hcom events sub "type='status' AND status_val='idle' AND instance='john'"

# Agent goes active→idle = finished their task. notification!
hcom send 'get back to work john!'
```


### Transcript Sharing

View conversation history for any instance:

```bash
hcom thread @john
```

Example use: background eavesdropping.

Prompt: `Create a hcom subscription for every time @john edits a file and review the code changes by looking at hcom thread`


### Launch terminal windows

`ENV_VARS hcom <N> [claude <ARGS...>]`

Launches terminals with claude connected to hcom

```bash
hcom 3                                       # 3 terminals with claude
hcom 3 claude -p                             # + headless
HCOM_TAG=api hcom 3 claude -p                # + @-mention group tag
hcom 3 claude --agent reviewer "review PR"   # + .claude/agents + prompt
etc...
```



### TUI Dashboard

A terminal UI:

![TUI Dashboard](https://raw.githubusercontent.com/aannoo/claude-hook-comms/refs/heads/assets/tui.png)

**Screens**

1. **MANAGE** — See all instances, status, send messages, start/stop instances

2. **LAUNCH** — Configure count, env vars, claude args, hcom settings

3. **EVENTS** — Live event stream and filter (messages, status changes, lifecycle)

   
---


## Examples

### Code + Reviewer Pattern

One instance writes code, another reviews in real-time:

```bash
# Terminal 1: coder
HCOM_TAG=coder hcom 1 'implement the cool feature. do a little bit then wait for review, fix, and continue'

# Terminal 2: reviewer (headless)
HCOM_TAG=reviewer hcom 1 claude -p 'analyze codebase then subscribe to events where coder status changes to idle, then review diff, send feedback via hcom'
```
The reviewer pings the coder: `@coder found big problem in cool.py line 42`


### Multi-Agent Debate


```bash
# Launch debaters with different roles
hcom 1 claude -p --agent role1
hcom 1 claude -p --agent role2
hcom 1 claude -p --agent role3

'You are the role1. Debate 5 rounds with role2 and role3 about this architecture decision...'
```


### Task Management

hcom provides real-time communication, other tools provide structure for long running / larger workflows.

Multiple instances in the same codebase; turn on collision detection: `hcom events sub collision`

#### Beads

[Beads](https://github.com/steveyegge/beads) is a dependency issue tracker.

**Identity** — link hcom name to bd audit trail:
```bash
hcom config name_export BD_ACTOR
```

**Notifications** — instances can subscribe to get notifications when bd activity occurs.
```bash
hcom events sub "status_detail LIKE 'bd close%'"      # work completed
hcom events sub "status_detail LIKE 'bd create%' OR status_detail LIKE 'bd close%' OR status_detail LIKE 'bd update%'" # state changes
```

#### Backlog.md

[Backlog](https://github.com/MrLesk/Backlog.md) is a task management tool.

Use CLI and subscribe instances to Bash tool events so they see what each other are doing in real time and can coordinate tasks.

```bash
hcom events sub "status_detail LIKE 'backlog %'" 
# all backlog activity, edit the sql to whatever bash commands you want to subscribe to
```

---

## Commands

| Command | Description |
|---------|-------------|
| `hcom` | TUI dashboard |
| `hcom <n>` | Launch `n` instances |
| `hcom start/stop` | Toggle participation |
| `hcom list` | View status, read receipts  |
| `hcom events` | View event history JSON|
| `hcom events sub/unsub` | Get "push" notifications in Claude |
| `hcom thread [name]` | View conversation transcript of instance |
| `hcom config` | Get/set `~/.hcom/config.env` values |
| `hcom relay hf [token]` | Setup cross-device chat |
| `hcom reset` | Archive and clear database

* `hcom --help` for more details

---

## Configuration

### Environment Variables

Set in `~/.hcom/config.env` or as environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `HCOM_TIMEOUT` | 1800 | Parent instance idle timeout (seconds) |
| `HCOM_SUBAGENT_TIMEOUT` | 30 | Subagent idle timeout (seconds) |
| `HCOM_TAG` | — | Group tag prefix for instance names |
| `HCOM_TERMINAL` | new | Terminal mode: `new` / `here` / custom |
| `HCOM_HINTS` | — | Text appended to all received messages |
| `HCOM_CLAUDE_ARGS` | — | Default Claude CLI arguments |
| `HCOM_NAME_EXPORT` | — | Export name to environment variable |

**Precedence:** environment variable > config.env > defaults

```bash
# Persist settings
hcom config timeout 3600

# One-time override
HCOM_TAG=poo hcom 2
```

---


## Platforms

### Claude Code Web

**1. Add these hooks to your repo in `.claude/settings.json`:**

<details>
<summary>hooks</summary>

```json
{
  "hooks": {
    "SessionStart": [{"hooks": [{"type": "command", "command": "if [ \"$CLAUDE_CODE_REMOTE\" = \"true\" ]; then pip install -q --no-cache-dir --root-user-action=ignore hcom; [ -n \"$HF_TOKEN\" ] && hcom relay hf; hcom sessionstart; fi"}]}],
    "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "if [ \"$CLAUDE_CODE_REMOTE\" = \"true\" ]; then hcom userpromptsubmit; fi"}]}],
    "PreToolUse": [{"matcher": "Bash|Task", "hooks": [{"type": "command", "command": "if [ \"$CLAUDE_CODE_REMOTE\" = \"true\" ]; then hcom pre; fi"}]}],
    "PostToolUse": [{"hooks": [{"type": "command", "command": "if [ \"$CLAUDE_CODE_REMOTE\" = \"true\" ]; then hcom post; fi", "timeout": 86400}]}],
    "Stop": [{"hooks": [{"type": "command", "command": "if [ \"$CLAUDE_CODE_REMOTE\" = \"true\" ]; then hcom poll; fi", "timeout": 86400}]}],
    "SubagentStart": [{"hooks": [{"type": "command", "command": "if [ \"$CLAUDE_CODE_REMOTE\" = \"true\" ]; then hcom subagent-start; fi"}]}],
    "SubagentStop": [{"hooks": [{"type": "command", "command": "if [ \"$CLAUDE_CODE_REMOTE\" = \"true\" ]; then hcom subagent-stop; fi", "timeout": 86400}]}],
    "Notification": [{"hooks": [{"type": "command", "command": "if [ \"$CLAUDE_CODE_REMOTE\" = \"true\" ]; then hcom notify; fi"}]}],
    "SessionEnd": [{"hooks": [{"type": "command", "command": "if [ \"$CLAUDE_CODE_REMOTE\" = \"true\" ]; then hcom sessionend; fi"}]}]
  },
  "env": {"HCOM": "hcom"}
}
```
</details>


**2. Configure environment** in Claude Code Web settings:
- Set `HF_TOKEN` - get a write token from [huggingface.co/settings/tokens](https://huggingface.co/settings/tokens)
- Set network access to Full

**3. In Claude Code Web**, prompt: `run hcom start`


### Mac/Windows/Linux

#### Defaults

- **macOS**: Terminal.app
- **Linux**: gnome-terminal, konsole, or xterm
- **Windows (native) & WSL**: Windows Terminal

#### Modes

- `HCOM_TERMINAL=new` - New terminal windows (default)
- `HCOM_TERMINAL=here` - Current terminal window

#### Use any terminal

HCOM generates a bash script containing env setup + claude command. Your custom terminal just needs to execute it. Use `{script}` as the placeholder for the script path.

<details>
<summary><strong>Custom Terminal Examples</strong></summary>

```bash
# Open Terminal.app or WT in new tab
HCOM_TERMINAL="ttab {script}"           # macOS: github.com/mklement0/ttab
HCOM_TERMINAL="wttab {script}"          # Windows: github.com/lalilaloe/wttab

# iTerm
HCOM_TERMINAL="open -a iTerm {script}"

# Wave Terminal Mac/Linux/Windows. From within Wave Terminal:
HCOM_TERMINAL="wsh run -- bash {script}"

# tmux with split panes and 3 claude instances in hcom chat
HCOM_TERMINAL="tmux split-window -h {script}" hcom 3

# Alacritty:
HCOM_TERMINAL="open -n -a Alacritty.app --args -e bash {script}" # macOS
HCOM_TERMINAL="alacritty -e bash {script}" # linux

# Kitty:
HCOM_TERMINAL="open -n -a kitty.app --args {script}" # macOS
HCOM_TERMINAL="kitty {script}" #Linux

# WezTerm 
HCOM_TERMINAL="wezterm start -- bash {script}" # Linux/Windows
HCOM_TERMINAL="open -n -a WezTerm.app --args start -- bash {script}" # macOS
HCOM_TERMINAL="wezterm cli spawn -- bash {script}" # Tabs from within WezTerm
HCOM_TERMINAL="/Applications/WezTerm.app/Contents/MacOS/wezterm cli spawn -- bash {script}" # Tabs from within WezTerm macOS
```
</details>


### Android

1. Install Termux from **F-Droid** (not Google Play)
2. Setup:
   ```bash
   pkg install python nodejs
   npm install -g @anthropic-ai/claude-cli
   pip install hcom
   ```
3. Enable external apps:
   ```bash
   echo "allow-external-apps=true" >> ~/.termux/termux.properties
   termux-reload-settings
   ```
4. Grant "Display over other apps" permission in Android settings
5. Run: `hcom 2`

---

## Reference

<details>
<summary><code>hcom --help</code> (all commands)</summary>

### Main Help
```
hcom v0.6.8 - Hook-based communication for Claude Code instances

Usage:
  hcom                               TUI dashboard
  [env vars] hcom <N> [claude ...]   Launch instances
  hcom <command>                     Run command

Commands:
  send      Send message to your buddies
  list      Show participants, status, read receipts
  start     Enable hcom participation
  stop      Disable hcom participation
  events    Query events / subscribe for push notifications
  thread    View other instance's conversation transcript
  config    Get/set config environment variables
  relay     Cross-device live chat
  reset     Archive and clear database or hooks

Run 'hcom <command> --help' for details.
```

### hcom send
```
Usage:
  hcom send "msg"         Send message to all your best buddies
  hcom send "@name msg"   Send to specific instance/group
    --from <name>      Identity for non-Claude tools (Gemini, scripts)
    --wait             Poll for @mentions (use with --from)
```

### hcom list
```
Usage:
  hcom list               All instances
  hcom list -v            Verbose output of all instances
  hcom list --json        Verbose JSON output of all instances

  hcom list [self|<name>] Instance details
    [field]            Print specific field (status, directory, session_id, etc)
    --json             Output as JSON
    --sh               Shell exports: eval "$(hcom list self --sh)"
```

### hcom start
```
Usage:
  hcom start              Enable hcom for current instance
  hcom start <name>       Re-enable stopped instance
```

### hcom stop
```
Usage:
  hcom stop               Disable hcom for current instance
  hcom stop <name>        Disable hcom for specific instance
  hcom stop all           Disable hcom for all instances
```

### hcom events
```
Usage:
  Query the event stream (messages, status changes, file edits, lifecycle)

Query:
    events             Recent events as JSON
    --last N           Limit count (default: 20)
    --sql EXPR         SQL WHERE filter (columns: id, timestamp, type, instance, data)
    --wait [SEC]       Block until match (default: 60s)

Subscribe:
    events sub         List subscriptions
    events sub "sql"   Create subscription for push notification when event matches SQL
    events sub collision Alert when instances edit same file
    --once             Auto-remove sub after first match
    --for <name>       Subscribe for another instance
    events unsub <id>  Remove by ID
    events unsub collision Disable collision alerts

Flat fields (events_v view):
    message            msg_from, msg_text, msg_scope, msg_sender_kind, msg_delivered_to, msg_mentions
    status             status_val, status_context, status_detail
    life               life_action, life_by, life_batch_id, life_reason

  Base columns: id, timestamp, type, instance
  Example: msg_from = 'alice' AND type = 'message'
  Use <> instead of != for SQL negation
```

### hcom thread
```
Usage:
  hcom thread             Show your conversation thread
  hcom thread @instance   See what another instance is doing
    --last N           Limit to last N exchanges (default: 10)
    --range N-M        Show exchanges N through M (1-indexed)
    --full             Show full assistant responses
    --detailed         Show tool I/O, edits, errors
    --json             JSON output
```

### hcom config
```
Usage:
  hcom config             Show all config values
  hcom config <key>       Get single config value
  hcom config <key> <val> Set config value
    --json             JSON output
    --edit             Open config in $EDITOR
    --reset            Reset config to defaults

Settings:
    HCOM_TAG           Group tag (creates tag-* instances)
    HCOM_TERMINAL      Terminal: new|here|"custom {script}"
    HCOM_HINTS         Text appended to messages received by instance
    HCOM_TIMEOUT       Idle timeout in seconds (default: 1800)
    HCOM_SUBAGENT_TIMEOUT Subagent timeout in seconds (default: 30)
    HCOM_CLAUDE_ARGS   Default claude args (e.g. "-p --model opus")
    HCOM_RELAY         Relay server URL
    HCOM_RELAY_TOKEN   Relay auth token
    HCOM_RELAY_ENABLED Enable relay sync (1|0)
    HCOM_NAME_EXPORT   Also export instance name to this var

  Non-HCOM_* vars in config.env pass through to Claude Code
  e.g. ANTHROPIC_MODEL=opus

Precedence: HCOM defaults < config.env < shell env vars
  Each resolves independently
```

### hcom relay
```
Usage:
  hcom relay              Show relay status
  hcom relay on           Enable cross-device chat
  hcom relay off          Disable cross-device chat
  hcom relay pull         Fetch from other devices now
  hcom relay hf [token]   Connect to relay server on HuggingFace
  (finds or creates a free private space on your HuggingFace account
  provide HF_TOKEN or login with hf cli first)
```

### hcom reset
```
Usage:
  hcom reset              Clear database (archive conversation)
  hcom reset hooks        Remove hooks only
  hcom reset all          Stop all + clear db + remove hooks + reset config
```

</details>


---

## License

MIT
