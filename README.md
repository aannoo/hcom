# hcom - Claude Hook Comms

[![PyPI - Version](https://img.shields.io/pypi/v/hcom)](https://pypi.org/project/hcom/)
[![PyPI - License](https://img.shields.io/pypi/l/hcom)](https://opensource.org/license/MIT) [![Python Version](https://img.shields.io/badge/python-3.10+-blue.svg)](https://python.org) [![DeepWiki](https://img.shields.io/badge/DeepWiki-aannoo%2Fclaude--hook--comms-blue.svg?logo=data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAACwAAAAyCAYAAAAnWDnqAAAAAXNSR0IArs4c6QAAA05JREFUaEPtmUtyEzEQhtWTQyQLHNak2AB7ZnyXZMEjXMGeK/AIi+QuHrMnbChYY7MIh8g01fJoopFb0uhhEqqcbWTp06/uv1saEDv4O3n3dV60RfP947Mm9/SQc0ICFQgzfc4CYZoTPAswgSJCCUJUnAAoRHOAUOcATwbmVLWdGoH//PB8mnKqScAhsD0kYP3j/Yt5LPQe2KvcXmGvRHcDnpxfL2zOYJ1mFwrryWTz0advv1Ut4CJgf5uhDuDj5eUcAUoahrdY/56ebRWeraTjMt/00Sh3UDtjgHtQNHwcRGOC98BJEAEymycmYcWwOprTgcB6VZ5JK5TAJ+fXGLBm3FDAmn6oPPjR4rKCAoJCal2eAiQp2x0vxTPB3ALO2CRkwmDy5WohzBDwSEFKRwPbknEggCPB/imwrycgxX2NzoMCHhPkDwqYMr9tRcP5qNrMZHkVnOjRMWwLCcr8ohBVb1OMjxLwGCvjTikrsBOiA6fNyCrm8V1rP93iVPpwaE+gO0SsWmPiXB+jikdf6SizrT5qKasx5j8ABbHpFTx+vFXp9EnYQmLx02h1QTTrl6eDqxLnGjporxl3NL3agEvXdT0WmEost648sQOYAeJS9Q7bfUVoMGnjo4AZdUMQku50McDcMWcBPvr0SzbTAFDfvJqwLzgxwATnCgnp4wDl6Aa+Ax283gghmj+vj7feE2KBBRMW3FzOpLOADl0Isb5587h/U4gGvkt5v60Z1VLG8BhYjbzRwyQZemwAd6cCR5/XFWLYZRIMpX39AR0tjaGGiGzLVyhse5C9RKC6ai42ppWPKiBagOvaYk8lO7DajerabOZP46Lby5wKjw1HCRx7p9sVMOWGzb/vA1hwiWc6jm3MvQDTogQkiqIhJV0nBQBTU+3okKCFDy9WwferkHjtxib7t3xIUQtHxnIwtx4mpg26/HfwVNVDb4oI9RHmx5WGelRVlrtiw43zboCLaxv46AZeB3IlTkwouebTr1y2NjSpHz68WNFjHvupy3q8TFn3Hos2IAk4Ju5dCo8B3wP7VPr/FGaKiG+T+v+TQqIrOqMTL1VdWV1DdmcbO8KXBz6esmYWYKPwDL5b5FA1a0hwapHiom0r/cKaoqr+27/XcrS5UwSMbQAAAABJRU5ErkJggg==)](https://deepwiki.com/aannoo/claude-hook-comms)

Real-time communication layer for Claude Code via hooks.

![Demo](https://raw.githubusercontent.com/aannoo/claude-hook-comms/main/screencapture.gif)

## Start

```bash
pip install hcom && hcom 2
```


## What

```
┌──────────┐  hcom send "hi"  ┌────────────────┐
│ Claude A │─────────────────►│ Claude B (idle)│──► wakes instantly:
└──────────┘        │         └────────────────┘           sees "hi"
- interactive       │
- headless          │         ┌──────────────────┐
- subagent          └────────►│Claude C (working)│──► after current tool:
- external tools              └──────────────────┘              sees "hi"
```

- Any Claude can join (`hcom start`) or leave (`hcom stop`) at runtime.
- Normal `claude` sessions are unaffected until you opt in.
- Works on Mac, Linux, Windows/WSL, Android, Claude Code Web.

## How

- Claude Code's Stop hook fires when Claude finishes responding.
- HCOM blocks there with `select()` turning idle into listening.
- TCP socket wakes instances instantly when messages arrive in SQLite.
- Hook exits with code 2 → stderr injected into Claude's context as a message.

**What gets installed:**
- `~/.hcom/` — database, config, logs
- `~/.claude/settings.json` — hooks

Safely remove with `hcom reset all`


## Commands

| Command | Description |
|---------|-------------|
| `hcom` | TUI dashboard |
| `hcom <n>` | Launch `n` instances |
| `hcom start/stop` | Toggle participation from any claude session |


---

## Features

<details>
<summary><strong>Instant Messaging</strong> — @mentions, groups, broadcasts</summary>

```bash
hcom send "hello everyone"          # Broadcast to all
hcom send "@john check this"        # Direct message

HCOM_TAG=backend hcom 2              # Creates backend-*, backend-*
HCOM_TAG=frontend hcom 2             # Creates frontend-*, frontend-*
hcom send "@backend scale up"        # Message entire backend group
```
</details>

<details>
<summary><strong>Persistent Headless Instances</strong></summary>

Run Claude instances in the background that stay alive waiting for follow up tasks

```bash
hcom 1 claude -p                     # Headless with 30min timeout
HCOM_TIMEOUT=3600 hcom 1 claude -p   # 1 hour timeout
hcom                                 # Monitor from dashboard

hcom 1 claude -p 'monitor [x] and send message via hcom if [y]'
```

</details>


<details>
<summary><strong>Subagent Communication</strong></summary>

When Claude uses the Task tool, subagents can run `hcom start` to get unique HCOM identities. They can communicate with each other during execution, and the parent sees the full conversation after completion.

```bash
# Inside Claude:
'use 3x task tool with task: debate x topic via hcom'
# Each subagent gets identity and can @mention each other during the task
```

</details>

<details>
<summary><strong>External Integration</strong> — any process can send/receive</summary>

Any process can send messages to Claude instances. Use `--from` to identify the sender. Use `--wait` to block until a reply (useful for external AI tools like Gemini or Codex).

```bash
# One-shot notification
hcom send --from ci-pipeline "build complete"

# Polling mode - block until Claude replies
hcom send --from gemini "analyze this code" --wait
# Returns when Claude responds, prints the reply
```

</details>

<details>
<summary><strong>Cross-Device Communication</strong></summary>

Relay messages between machines via HuggingFace Spaces (free, private).

**Setup:**
```bash
hcom relay hf <token>      # Get write token from huggingface.co/settings/tokens
hcom relay hf              # Or if already logged in via `huggingface-cli login`
```
Creates a private Space on your HuggingFace account (~15 sec build).

Run on each device. Remote instances appear in the TUI as `name:XXXX`.

**Status:**
```bash
hcom relay                 # Check sync status
hcom relay on/off          # Enable/disable
```

</details>

<details>
<summary><strong>Dynamic Opt-In/Out</strong></summary>

Any Claude Code instance can join or leave the chat at runtime. Normal `claude` stays unaffected until you explicitly opt in. Stopping doesn't kill the instance—it just disconnects from HCOM.

```bash
# From inside Claude Code:
'run hcom start'                     # Join the chat
'run hcom stop'                      # Leave chat, keep working

# From outside:
hcom start alice                     # Enable specific instance
hcom stop all                        # Disconnect all instances
```

</details>

## Config

<details>
<summary><strong>Claude Code Web</strong></summary>

**1. Add hooks to your repo** (`.claude/settings.json`):
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

**2. Configure environment** in Claude Code Web settings:
- Set `HF_TOKEN` - get a write token from [huggingface.co/settings/tokens](https://huggingface.co/settings/tokens)
- Set network access to **Full** (relay needs huggingface.co, not in default allowlist)

A private HuggingFace Space is auto-created on first session.

**3. In Claude Code Web**, prompt: `run hcom start`

</details>

<details>
<summary><strong>Custom Terminal</strong></summary>

### Defaults

- **macOS**: Terminal.app
- **Linux**: gnome-terminal, konsole, or xterm
- **Windows (native) & WSL**: Windows Terminal
- **Android**: Termux

### Modes

- `HCOM_TERMINAL=new` - New terminal windows (default)
- `HCOM_TERMINAL=here` - Current terminal window
- `HCOM_TERMINAL="open -a iTerm {script}"` - Custom terminal

### Setup

HCOM generates a bash script containing env setup + claude command. Your custom terminal just needs to execute it. Use `{script}` as the placeholder for the script path.

#### Examples

```bash
# Open Terminal.app or WT in new tab
HCOM_TERMINAL="ttab {script}"              # macOS: github.com/mklement0/ttab
HCOM_TERMINAL="wttab {script}"             # Windows: github.com/lalilaloe/wttab

# Wave Terminal Mac/Linux/Windows. From within Wave Terminal:
HCOM_TERMINAL="wsh run -- bash {script}"

# Alacritty macOS:
HCOM_TERMINAL="open -n -a Alacritty.app --args -e bash {script}"

# Alacritty Linux:
HCOM_TERMINAL="alacritty -e bash {script}"

# Kitty macOS:
HCOM_TERMINAL="open -n -a kitty.app --args {script}"

# Kitty Linux
HCOM_TERMINAL="kitty {script}"

# tmux with split panes and 3 claude instances in hcom chat
HCOM_TERMINAL="tmux split-window -h {script}" hcom 3

# WezTerm Linux/Windows
HCOM_TERMINAL="wezterm start -- bash {script}"

# Tabs from within WezTerm
HCOM_TERMINAL="wezterm cli spawn -- bash {script}"

# WezTerm macOS:
HCOM_TERMINAL="open -n -a WezTerm.app --args start -- bash {script}"

# Tabs from within WezTerm macOS
HCOM_TERMINAL="/Applications/WezTerm.app/Contents/MacOS/wezterm cli spawn -- bash {script}"
```
</details>

<details>
<summary><strong>Android</strong></summary>

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

4. Grant "Display over other apps" permission in Android settings for visible terminals

5. Run: `hcom 2`

</details>

<details>
<summary><strong>Environment Variables</strong></summary>

Settings in `~/.hcom/config.env` or environment variables.

| Variable | Default | Description |
|----------|---------|-------------|
| `HCOM_TIMEOUT` | 1800 | Instance idle timeout (seconds) |
| `HCOM_SUBAGENT_TIMEOUT` | 30 | Subagent idle timeout (seconds) |
| `HCOM_TAG` | — | Group tag (creates tag-* instances) |
| `HCOM_TERMINAL` | new | Terminal mode: new\|here\|print\|custom |
| `HCOM_HINTS` | — | Text appended to all received messages |
| `HCOM_CLAUDE_ARGS` | — | Default Claude CLI arguments (e.g., `--agent reviewer`) |

**Precedence**: env var > config.env > defaults

```bash
# Persist settings
hcom config timeout 3600
hcom config tag backend

# One-time override
HCOM_TAG=api hcom 2
```

</details>

---

## Reference

<details>
<summary><code>hcom --help</code></summary>

```bash
Usage: hcom                           # TUI dashboard
       [ENV_VARS] hcom <COUNT> [claude <ARGS>...]
       hcom events [--last N] [--wait SEC] [--sql EXPR]
       hcom list [--json] [-v|--verbose]
       hcom send "message"
       hcom stop [alias|all]
       hcom start [alias]
       hcom reset [hooks|all]

Launch Examples:
  hcom 3             open 3 terminals with claude connected to hcom
  hcom 3 claude -p                                       + headless
  HCOM_TAG=api hcom 3 claude -p               + @-mention group tag
  claude 'run hcom start'        claude code with prompt also works

Commands:
  events              Query recent events (JSON)
    --last N          Limit to last N events (default: 20)
    --wait [SEC]      Block until matching event (default: 60s timeout)
    --sql EXPR        SQL WHERE clause filter

  list                List current instances status
    -v, --verbose     Show detailed metadata
    --json            Emit JSON with detailed data

  send "msg"          Send message to all instances
  send "@alias msg"   Send to specific instance/group
    --from <name>     Custom external identity
    --wait            Block until reply with --from

  stop                Stop current instance (from inside Claude)
  stop <alias>        Stop specific instance
  stop all            Stop all instances

  start               Start current instance (from inside Claude)
  start <alias>       Start specific instance

  reset               Clear database (archive conversation)
  reset hooks         Remove hooks only
  reset all           Stop all + clear db + remove hooks + reset config

  config              Show all config settings
  config <key>        Get single config value
  config <key> <val>  Set config value
    --json            JSON output
    --edit            Open config in $EDITOR
    --reset           Reset config to defaults

  relay               Show relay status
  relay on            Enable relay sync
  relay off           Disable relay sync
  relay pull          Manual relay pull
  relay hf [token]    Setup HuggingFace relay

Environment Variables:
  HCOM_TAG=name               Group tag (creates name-* instances)
  HCOM_TERMINAL=mode          Terminal: new|here|print|"custom {script}"
  HCOM_HINTS=text             Text appended to all messages received by instance
  HCOM_TIMEOUT=secs           Time until disconnected from hcom chat (default: 1800s / 30m)
  HCOM_SUBAGENT_TIMEOUT=secs  Subagent idle timeout (default: 30s)
  HCOM_CLAUDE_ARGS=args       Claude CLI defaults (e.g., '-p --model opus --agent reviewer')

  ANTHROPIC_MODEL=opus # Any env var passed through to Claude Code

  Persist Env Vars in `~/.hcom/config.env` or use `hcom config`
```
</details>


---

## License

MIT

