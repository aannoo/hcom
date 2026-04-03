# hcom - multi-agent communication

[![PyPI](https://img.shields.io/pypi/v/hcom)](https://pypi.org/project/hcom/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/Built_with-Rust-dea584)](https://www.rust-lang.org/)
[![GitHub stars](https://img.shields.io/github/stars/aannoo/hcom)](https://github.com/aannoo/hcom/stargazers)

**Let AI agents message, watch, and spawn each other across terminals.**

Agents running in separate terminals are isolated. Context doesn't transfer, decisions get repeated, file edits collide.

Prefix any agent with `hcom` and they're connected. When one agent edits a file, runs a command, or sends a message, other agents can find out immediately.

Works with Claude Code, Gemini CLI, Codex, OpenCode, and any AI tool that can run shell commands.

![demo](https://github.com/aannoo/hcom/releases/download/v0.6.8/screencapture-new-new.gif)

---

## Install

```bash
brew install aannoo/hcom/hcom
```

<details><summary>Or with curl/pip/uv</summary>

```bash
# Direct installer works on macOS, Linux, Android (Termux), and WSL
curl -fsSL https://github.com/aannoo/hcom/releases/latest/download/hcom-installer.sh | sh
```

```bash
# With uv
uv tool install hcom
# Or with pip
pip install hcom
```

</details>

---

## Quickstart

Run agents with `hcom` in front:

```bash
hcom claude
hcom gemini
hcom codex
hcom opencode
```

Prompt:

> send a message to claude

Open the TUI:

```bash
hcom
```

---

## How it works

Messages arrive mid-turn or wake idle agents immediately.

If 2 agents edit the same file within 30 seconds, both get collision notifications.

Refer to agents by name, tool, terminal, branch, cwd, or set a custom tag.

```bash
# hooks record activity and deliver messages
agent → hooks → db → hooks → other agent
```

---


## What you can do

Tell any agent:

> when codex goes idle send it the next task

> watch gemini's file edits, review each and send feedback if any bugs

> fork yourself to investigate the bug and report back

> find which agent worked on terminal_id code, resume them and ask why it sucks


## What agents can do

|Capability|Command|
|---|---|
| Message each other (intents, threads, broadcast, @mentions) | `hcom send` |
| Read each other's transcripts (ranges and detail levels) | `hcom transcript` |
| View terminal screens, inject text/enter for approvals | `hcom term` |
| Query event history (file edits, commands, status, lifecycle) | `hcom events` |
| Subscribe and react to each other's activity | `hcom events sub` |
| Spawn (multiple), fork, resume agents in new terminal panes | `hcom N claude\|gemini\|codex\|opencode`, `hcom r`, `hcom f` |
| Kill agents and close their terminal panes/sessions | `hcom kill` |
| Build context bundles (files, transcript, events) for handoffs | `hcom bundle` |

---

## Multi-agent workflows

Included workflow scripts.

**`hcom run confess`** - An agent (or background clone) writes an honesty self-eval. A spawned calibrator reads the target's transcript independently. A judge compares both reports and sends back a verdict via hcom message.

**`hcom run debate`** - A judge spawns and sets up a debate with existing agents. It coordinates rounds in a shared thread where all agents see each other's arguments, with shared context of workspace files and transcripts.

**`hcom run fatcow`** — headless agent reads every file in a path, subscribes to file edit events to stay current, and answers other agents on demand.

Create your own by prompting:

> "read `hcom run docs` then make a script that does X"

---

## Tools

**Claude Code, Gemini CLI, Codex, OpenCode** — messages are delivered automatically, events are tracked.

**Any AI tool that can run shell commands** - send and receive messages manually. Tell agent "run `hcom start`"

**Any process** - fire and forget: `hcom send <message> --from botname`

<details><summary>Claude Code headless</summary>

Detached background process that stays alive. Manage via TUI.

```bash
hcom claude -p 'say hi in hcom'
```
</details>

<details><summary>Claude Code subagents</summary>

Run `hcom claude`. Then inside, prompt:

> run 2x task tool and get them to talk to each other in hcom

</details>

---

## Terminal
 
Spawning works with any terminal emulator. Killing/closing works with **kitty**, **wezterm**, and **tmux**.

Run for more info / custom setup: `hcom config terminal --info`
 
```bash
hcom config terminal default       # auto-detect
hcom config terminal kitty         # set
hcom claude --terminal tmux        # override once
```
 
---

## Cross-device

Connect agents across machines via MQTT relay:

```bash
# get token
hcom relay new

# on each device
hcom relay connect <token>
```

---

## What gets installed

Hooks go into `~/` (or `HCOM_DIR`) on first run. If you aren't using hcom, the hooks do nothing.

```bash
hcom hooks remove                  # safely remove only hcom hooks
hcom status                        # diagnostics
```

```bash
HCOM_DIR=$PWD/.hcom                # for sandbox or project local
```

---

## Reference

<details>
<summary>CLI</summary>

```
hcom (hook-comms) v0.7.8 - multi-agent communication

Usage:
hcom                                  TUI dashboard
hcom <command>                        Run command

Launch:
hcom [N] claude|gemini|codex|opencode [flags] [tool-args]
hcom r <name>                         Resume stopped agent
hcom f <name>                         Fork agent session
hcom kill <name(s)|tag:T|all>         Kill + close terminal pane

Commands:
send         Send message to your buddies
listen       Block until message or event arrives
list         Show agents, status, unread counts
events       Query event stream, manage subscriptions
bundle       Structured context packages for handoffs
transcript   Read another agent's conversation
start        Connect to hcom (run inside any AI tool)
stop         Disconnect from hcom
config       Get/set global and per-agent settings
run          Execute workflow scripts
relay        Cross-device sync + relay daemon
archive      Query past hcom sessions
reset        Archive and clear database
hooks        Add or remove hooks
status       Installation and diagnostics
term         View/inject into agent PTY screens
update       Check and apply updates


## send

Usage:

Usage:
    send @name -- message text     Direct message
    send @name1 @name2 -- message  Multiple targets
    send -- message text           Broadcast to all
    send @name                     Message from stdin (pipe or heredoc)
    send @name --file <path>       Message from file
    send @name --base64 <encoded>  Message from base64 string

  Everything after -- is the message (no quotes needed).
  All flags must come before --.


Target matching:
    @luna                          base name (matches luna, api-luna)
    @api-luna                      exact full name
    @api-                          prefix: all with tag 'api'
    @luna:BOXE                     remote agent on another device
  Underscore blocks prefix: @luna does NOT match luna_reviewer_1


Envelope:
    --intent <type>                request | inform | ack
    request: expect a response
    inform: FYI, no response needed
    ack: replying to a request (requires --reply-to)
    --reply-to <id>                Link to event ID (42 or 42:BOXE)
    --thread <name>                Group related messages


Sender:
    --from <name>                  External sender identity (alias: -b)
    --name <name>                  Your identity (agent name or UUID)


Inline bundle (attach structured context):
    --title <text>                 Create and attach bundle inline
    --description <text>           Bundle description (required with --title)
    --events <ids>                 Event IDs/ranges: 1,2,5-10
    --files <paths>                Comma-separated file paths
    --transcript <ranges>          Format: 3-14:normal,6:full,22-30:detailed
    --extends <id>                 Parent bundle (optional)
  See 'hcom bundle --help' for bundle details


Examples:
    hcom send @luna -- Hello there! 
    hcom send @luna @nova --intent request -- Can you help? 
    hcom send -- Broadcast message to everyone 
    echo 'Complex message' | hcom send @luna 
    hcom send @luna <<'EOF'        
    Multi-line message with special chars 
    EOF                            

## list

Usage:
  hcom list                       All alive agents, read receipts
    -v                             Verbose (directory, session, etc)
    --json                         JSON array of all agents
    --names                        Just names, one per line
    --format TPL                   Template per agent: --format '{name} {status}'
    Fields: name, base_name, status, status_context, status_detail,
    status_age_seconds, description, unread_count, tool, tag, directory,
    session_id, parent_name, agent_id, headless, created_at,
    hooks_bound, process_bound, transcript_path, background_log_file,
    launch_context

  hcom list [self|<name>]         Single agent details
    [field]                        Print specific field (status, directory, session_id, ...)
    --json                         Output as JSON
    --sh                           Shell exports: eval "$(hcom list self --sh)"

  hcom list --stopped [name]      Stopped agents (from events)
    --all                          All stopped (default: last 20)


Status icons:
  ▶  active      processing, reads messages very soon
  ◉  listening   idle, reads messages in <1s
  ■  blocked     needs human approval
  ○  inactive    dead or stale
  ◦  unknown     neutral


Tool labels:
  [CLAUDE] [GEMINI] [CODEX] [OPENCODE]  hcom-launched (PTY + hooks)
  [claude] [gemini] [codex] [opencode]  vanilla (hooks only)
  [AD-HOC]                              manual polling

## events

Usage:
  Query the event stream (messages, status changes, file edits, lifecycle)


Query:
    events                         Last 20 events as JSON
    --last N                       Limit count (default: 20)
    --all                          Include archived sessions
    --wait [SEC]                   Block until match (default: 60s)
    --sql EXPR                     Raw SQL WHERE (ANDed with flags)


Filters (same flag repeated = OR, different flags = AND):
    --agent NAME                   Agent name
    --type TYPE                    message | status | life
    --status VAL                   listening | active | blocked
    --context PATTERN              tool:Bash | deliver:X (supports * wildcard)
    --action VAL                   created | started | ready | stopped | batch_launched
    --cmd PATTERN                  Shell command (contains, ^prefix, $suffix, =exact, *glob)
    --file PATH                    File write (*.py for glob, file.py for contains)
    --collision                    Two agents edit same file within 30s
    --from NAME                    Sender
    --mention NAME                 @mention target
    --intent VAL                   request | inform | ack
    --thread NAME                  Thread name
    --after TIME                   After timestamp (ISO-8601)
    --before TIME                  Before timestamp (ISO-8601)


Shortcuts:
    --idle NAME                    --agent NAME --status listening
    --blocked NAME                 --agent NAME --status blocked


Subscribe (next matching event delivered as messages from [hcom-events]):
    events sub list                List active subscriptions
    events sub [filters] [--once]  Subscribe using filter flags (listed above)
    events sub "SQL WHERE" [--once] Subscribe using raw SQL
      --once                       Auto-remove after first match
      --for <name>                 Subscribe on behalf of another agent
    events unsub <id>              Remove subscription


Examples:
    events --cmd git --agent peso  
    events sub --idle peso         Notified when peso goes idle
    events sub --file '*.py' --once One-shot: next .py file write


SQL reference (events_v view):
    Base                           id, timestamp, type, instance
    msg_*                          from, text, scope, sender_kind, delivered_to[], mentions[], intent, thread, reply_to
    status_*                       val, context, detail
    life_*                         action, by, batch_id, reason

    type                           message, status, life
    msg_scope                      broadcast, mentions
    msg_sender_kind                instance, external, system
    status_context                 tool:X, deliver:X, approval, prompt, exit:X
    life_action                    created, ready, stopped, batch_launched

  delivered_to/mentions are JSON arrays — use LIKE '%name%' not = 'name'
  Use <> instead of != for SQL negation

## stop

Usage:
  hcom stop                       Disconnect self from hcom
  hcom stop <name>                Disconnect specific agent
  hcom stop <n1> <n2> ...         Disconnect multiple
  hcom stop tag:<name>            Disconnect all with tag
  hcom stop all                   Disconnect all agents


## start

Usage:
  hcom start                      Connect to hcom (from inside any AI session)
  hcom start --as <name>          Reclaim identity (after compaction/resume/clear)
  hcom start --orphan <name|pid>  Recover orphaned PTY process from pidtrack


  Inside a sandbox? Prefix all hcom commands with: HCOM_DIR=$PWD/.hcom

## listen

Usage:
  hcom listen [timeout]           Block until message arrives
    [timeout]                      Timeout in seconds (alias for --timeout)
    --timeout N                    Timeout in seconds (default: 86400)
    --json                         Output messages as JSON


Filter flags:
  Supports all filter flags from 'events' command
  (--agent, --type, --status, --file, --cmd, --from, --intent, etc.)
  Run 'hcom events --help' for full list
  Filters combine with --sql using AND logic


SQL filter mode:
    --sql "type='message'"         Custom SQL against events_v
    --sql stopped:name             Preset: wait for agent to stop
    --idle NAME                    Shortcut: wait for agent to go idle


Exit codes:
    0                              Message received / event matched
    1                              Timeout or error

  Quick unread check: hcom listen 1

## status

Usage:
  hcom status                     Installation status and diagnostics
  hcom status --logs              Include recent errors and warnings
  hcom status --json              Machine-readable output

## config

Usage:
  hcom config                     Show effective config (with sources)
  hcom config <key>               Get one key
  hcom config <key> <value>       Set one key
  hcom config <key> --info        Detailed help for a key
    --json / --edit / --reset      


Per-agent:
  hcom config -i <name|self> [key] [val] tag, timeout, hints, subagent_timeout


Keys:
    tag                            Group/label (agents become tag-*)
    terminal                       Where new agent windows open
    hints                          Text appended to all messages agent receives
    notes                          Notes appended to agent bootstrap
    subagent_timeout               Subagent keep-alive seconds after task
    claude_args / gemini_args / codex_args / opencode_args 
    auto_approve                   Auto-approve safe hcom commands
    auto_subscribe                 Event auto-subscribe presets
    name_export                    Export agent name to custom env var
  hcom config <key> --info for details

  Precedence: defaults < config.toml < env vars
  Files: ~/.hcom/config.toml, ~/.hcom/config.env
  HCOM_DIR: isolate per project (see 'hcom reset --help')

## hooks

Usage:
  hcom hooks                      Show hook status
  hcom hooks status               Same as above
  hcom hooks add [tool]           Add hooks (claude | gemini | codex | opencode | all)
  hcom hooks remove [tool]        Remove hooks (claude | gemini | codex | opencode | all)

  Hooks enable automatic message delivery and status tracking.
  Without hooks, use ad-hoc mode (run hcom start inside any AI tool).
  Restart the tool after adding hooks to activate.
  Remove cleans both global (~/) and HCOM_DIR-local if set.

## archive

Usage:
  hcom archive                    List archived sessions (numbered)
  hcom archive <N>                Query events from archive (1 = most recent)
  hcom archive <N> agents         Query agents from archive
  hcom archive <name>             Query by stable name (prefix match)
    --here                         Filter to archives from current directory
    --sql "expr"                   SQL WHERE filter
    --last N                       Limit events (default: 20)
    --json                         JSON output

## reset

Usage:
  hcom reset                      Archive conversation, clear database
  hcom reset all                  Stop all + clear db + remove hooks + reset config


Sandbox / local mode:
  If you can't write to ~/.hcom, set:
    export HCOM_DIR="$PWD/.hcom"
  Hooks install under $PWD (.claude/.gemini/.codex) or ~/.config/opencode/, state in $HCOM_DIR

  To remove local setup:
    hcom hooks remove && rm -rf "$HCOM_DIR"

  Explicit location:
    export HCOM_DIR=/your/path/.hcom


## transcript

Usage:
  hcom transcript <name>          View agent's conversation (last 10)
  hcom transcript <name> N        Show exchange N
  hcom transcript <name> N-M      Show exchanges N through M
  hcom transcript timeline        User prompts across all agents by time
    --last N                       Limit to last N exchanges (default: 10)
    --full                         Show complete assistant responses
    --detailed                     Show tool I/O, file edits, errors
    --json                         JSON output

  hcom transcript search "pattern" Search hcom-tracked transcripts (rg/grep)
    --live                         Only currently alive agents
    --all                          All transcripts (includes non-hcom sessions)
    --limit N                      Max results (default: 20)
    --agent TYPE                   Filter: claude | gemini | codex | opencode
    --exclude-self                 Exclude the searching agent's own transcript
    --json                         JSON output

  Tip: Reference ranges in messages instead of copying:
  "read my transcript range 7-10 --full"

## bundle

Usage:
  hcom bundle                     List recent bundles (alias: bundle list)
  hcom bundle list                List recent bundles
    --last N                       Limit count (default: 20)
    --json                         Output JSON

  hcom bundle cat <id>            Expand full bundle content
  Shows: metadata, files (metadata only), transcript (respects detail level), events

  hcom bundle prepare             Show recent context, suggest template
    --for <agent>                  Prepare for specific agent (default: self)
    --last-transcript N            Transcript entries to suggest (default: 20)
    --last-events N                Events to scan per category (default: 30)
    --json                         Output JSON
    --compact                      Hide how-to section
  Shows suggested transcript ranges, relevant events, files
  Outputs ready-to-use bundle create command
  TIP: Skip 'bundle create' — use bundle flags directly in 'hcom send'

  hcom bundle show <id>           Show bundle by id/prefix
    --json                         Output JSON

  hcom bundle create "title"      Create bundle (positional or --title)
    --title <text>                 Bundle title (alternative to positional)
    --description <text>           Bundle description (required)
    --events 1,2,5-10              Event IDs/ranges, comma-separated (required)
    --files a.py,b.py              Comma-separated file paths (required)
    --transcript RANGES            Transcript with detail levels (required)
      Format: range:detail (3-14:normal,6:full,22-30:detailed)
      normal = truncated | full = complete | detailed = tools+edits
    --extends <id>                 Parent bundle for chaining
    --bundle JSON                  Create from JSON payload
    --bundle-file FILE             Create from JSON file
    --json                         Output JSON


JSON format:
  {
    "title": "Bundle Title",
    "description": "What happened, decisions, state, next steps",
    "refs": {
      "events": ["123", "124-130"],
      "files": ["src/auth.py", "tests/test_auth.py"],
      "transcript": ["10-15:normal", "20:full", "30-35:detailed"]
    },
    "extends": "bundle:abc123"
  }

  hcom bundle chain <id>          Show bundle lineage
    --json                         Output JSON

## kill

Usage:
  hcom kill <name>                Kill process (+ close terminal pane)
  hcom kill tag:<name>            Kill all with tag
  hcom kill all                   Kill all with tracked PIDs


## term

Usage:
  hcom term                       Screen dump (all PTY instances)
  hcom term [name]                Screen dump for specific agent
    --json                         Raw JSON output

  hcom term inject <name> [text]  Inject text into agent PTY
    --enter                        Append \r (submit). Works alone or with text.

  hcom term debug on              Enable PTY debug logging (all instances)
  hcom term debug off             Disable PTY debug logging
  hcom term debug logs            List debug log files


JSON fields: lines[], size[rows,cols], cursor[row,col],
  ready, prompt_empty, input_text

  Debug toggle; instances detect within ~10s.
  Logs: ~/.hcom/.tmp/logs/pty_debug/

## relay

Usage:
  hcom relay                      Show status and token
  hcom relay new                  Create new relay group
  hcom relay connect <token>      Join relay group
  hcom relay on / off             Enable/disable sync


Setup:
  1. 'relay new' to get token
  2. 'relay connect <token>' on each device


Custom broker:
  hcom relay new --broker mqtts://host:port --password <broker-auth-secret> 
  hcom relay connect <token> --password <secret> 


Daemon:
  hcom relay daemon               Show daemon status
  hcom relay daemon start         Start the relay daemon
  hcom relay daemon stop          Stop the daemon (SIGKILL after 5s)
  hcom relay daemon restart       Restart the daemon


## run

Usage:
  hcom run                        List available workflow/launch scripts and more info
  hcom run <name> [args]          Execute script
  hcom run <name> --help          Script options
  hcom run docs                   CLI reference + config + script creation guide

  Docs sections:
    hcom run docs --cli            CLI reference only
    hcom run docs --config         Config settings only
    hcom run docs --scripts        Script creation guide

  User scripts: ~/.hcom/scripts/

## update

Usage:
  hcom update                     Check for and apply updates
  hcom update --check             Only check — print status without applying

  Detects install method and runs the right update command:
    brew install    → brew upgrade hcom
    uv tool install → uv tool upgrade hcom
    pip install     → pip install -U hcom
    curl installer  → re-run hcom-installer.sh

## claude

Usage:
  hcom [N] claude [args...]       Launch N Claude agents (default N=1)

    hcom claude                        Runs in current terminal
    hcom 3 claude                      Opens 3 new terminal windows
    hcom 3 claude -p "prompt"          3 headless in background
    hcom 1 claude --agent <name>       .claude/agents/<name>.md

hcom Flags:
    --tag <name>                 Group tag (names become tag-*)
    --terminal <preset>          Where new windows open
    --hcom-prompt <text>          Initial prompt
    --hcom-system-prompt <text>   System prompt

Environment:
    HCOM_CLAUDE_ARGS             Default args (merged with CLI)
    HCOM_TAG                     Group tag (agents become tag-*)
    HCOM_TERMINAL                default | <preset> | "cmd {script}"
    HCOM_HINTS                   Appended to messages received
    HCOM_NOTES                   One-time bootstrap notes
    HCOM_SUBAGENT_TIMEOUT        Seconds subagents keep-alive after task

Resume / Fork:
    hcom r <name>                  Resume stopped agent by name
    hcom f <name>                  Fork agent session (active or stopped)

  Run "claude --help" for claude options.
  Run "hcom config terminal --info" for terminal presets.

## gemini

Usage:
  hcom [N] gemini [args...]       Launch N Gemini agents (default N=1)

    hcom gemini                        Runs in current terminal
    hcom 3 gemini                      Opens 3 new terminal windows
    hcom N gemini --yolo               Flags forwarded to gemini

hcom Flags:
    --tag <name>                 Group tag (names become tag-*)
    --terminal <preset>          Where new windows open
    --hcom-prompt <text>          Initial prompt
    --hcom-system-prompt <text>   System prompt

Environment:
    HCOM_GEMINI_ARGS             Default args (merged with CLI)
    HCOM_TAG                     Group tag (agents become tag-*)
    HCOM_TERMINAL                default | <preset> | "cmd {script}"
    HCOM_HINTS                   Appended to messages received
    HCOM_NOTES                   One-time bootstrap notes
    HCOM_GEMINI_SYSTEM_PROMPT    System prompt (env var)

Resume:
    hcom r <name>                  Resume stopped agent by name
  Gemini does not support session forking (hcom f).

  Run "gemini --help" for gemini options.
  Run "hcom config terminal --info" for terminal presets.

## codex

Usage:
  hcom [N] codex [args...]       Launch N Codex agents (default N=1)

    hcom codex                         Runs in current terminal
    hcom 3 codex                       Opens 3 new terminal windows
    hcom codex --sandbox danger-full-access Flags forwarded to codex

hcom Flags:
    --tag <name>                 Group tag (names become tag-*)
    --terminal <preset>          Where new windows open
    --hcom-prompt <text>          Initial prompt
    --hcom-system-prompt <text>   System prompt

Environment:
    HCOM_CODEX_ARGS              Default args (merged with CLI)
    HCOM_TAG                     Group tag (agents become tag-*)
    HCOM_TERMINAL                default | <preset> | "cmd {script}"
    HCOM_HINTS                   Appended to messages received
    HCOM_NOTES                   One-time bootstrap notes
    HCOM_CODEX_SYSTEM_PROMPT     System prompt (env var or config)

Resume / Fork:
    hcom r <name>                  Resume stopped agent by name
    hcom f <name>                  Fork agent session (active or stopped)

  Run "codex --help" for codex options.
  Run "hcom config terminal --info" for terminal presets.

## opencode

Usage:
  hcom [N] opencode [args...]       Launch N OpenCode agents (default N=1)

    hcom opencode                      Runs in current terminal
    hcom 3 opencode                    Opens 3 new terminal windows

hcom Flags:
    --tag <name>                 Group tag (names become tag-*)
    --terminal <preset>          Where new windows open
    --hcom-prompt <text>          Initial prompt
    --hcom-system-prompt <text>   System prompt

Environment:
    HCOM_OPENCODE_ARGS           Default args (merged with CLI)
    HCOM_TAG                     Group tag (agents become tag-*)
    HCOM_TERMINAL                default | <preset> | "cmd {script}"
    HCOM_HINTS                   Appended to messages received
    HCOM_NOTES                   One-time bootstrap notes

Resume / Fork:
    hcom r <name>                  Resume stopped agent by name
    hcom f <name>                  Fork agent session (active or stopped)

  Run "opencode --help" for opencode options.
  Run "hcom config terminal --info" for terminal presets.
```

</details>

<details>
<summary>Config</summary>

```
# Config Settings

File: ~/.hcom/config.toml
Precedence: defaults < config.toml < env vars

Commands:
  hcom config                 Show all values
  hcom config <key> <val>     Set value
  hcom config <key> --info    Detailed help for a setting
  hcom config --edit          Open in $EDITOR

HCOM_TAG - Group tag for launched instances

Purpose:
  Creates named groups of agents that can be addressed together.
  When set, launched instances get names like: <tag>-<name>

Usage:
  hcom config tag myteam        # Set tag
  hcom config tag ""            # Clear tag

  # Or via environment:
  HCOM_TAG=myteam hcom 3 claude

Effect:
  Without tag: launches create → luna, nova, kira
  With tag "dev": launches create → dev-luna, dev-nova, dev-kira

Addressing:
  @dev         → sends to all agents with tag "dev"
  @dev-luna    → sends to specific agent

Allowed characters: letters, numbers, hyphens (a-z, A-Z, 0-9, -)

HCOM_HINTS - Text injected with all messages

Purpose:
  Appends text to every message received by launched agents.
  Useful for persistent instructions or context.

Usage:
  hcom config hints "Always respond in JSON format"
  hcom config hints ""   # Clear hints

Example:
  hcom config hints "You are part of team-alpha. Coordinate with @team-alpha members."

Notes:
  - Hints are appended to message content, not system prompt
  - Each agent can have different hints (set via hcom config -i <name> hints)
  - Global hints apply to all new launches

HCOM_NOTES - One-time notes appended to bootstrap

  Custom text added to agent system context at startup.
  Unlike HCOM_HINTS (per-message), this is injected once and does not repeat.

Usage:
  hcom config notes "Always check hcom list before spawning new agents"
  hcom config notes ""                            # Clear
  HCOM_NOTES="tips" hcom 1 claude                 # Per-launch override

  Changing after launch has no effect (bootstrap already delivered).

HCOM_TIMEOUT - Advanced: idle timeout for headless/vanilla Claude (seconds)

Default: 86400 (24 hours)

This setting only applies to:
  - Headless Claude: hcom N claude -p
  - Vanilla Claude: claude + hcom start

Does NOT apply to:
  - Interactive PTY mode: hcom N claude (main path)
  - Gemini or Codex

How it works:
  - Claude's Stop hook runs when Claude goes idle
  - Hook waits up to TIMEOUT seconds for a message
  - If no message within timeout, instance is unregistered

Usage (if needed):
  hcom config HCOM_TIMEOUT 3600   # 1 hour
  export HCOM_TIMEOUT=3600        # via environment

HCOM_SUBAGENT_TIMEOUT - Timeout for Claude subagents (seconds)

Default: 30

Purpose:
  How long Claude waits for a subagent (Task tool) to complete.
  Shorter than main timeout since subagents should be quick.

Usage:
  hcom config subagent_timeout 60    # 1 minute
  hcom config subagent_timeout 30    # 30 seconds (default)

Notes:
  - Only applies to Claude Code's Task tool spawned agents
  - Parent agent blocks until subagent completes or times out
  - Increase for complex subagent tasks

HCOM_CLAUDE_ARGS - Default args passed to claude on launch

Example: hcom config claude_args "--model opus"
Clear:   hcom config claude_args ""

Merged with launch-time cli args (launch args win on conflict).

HCOM_GEMINI_ARGS - Default args passed to gemini on launch

Example: hcom config gemini_args "--model gemini-2.5-flash"
Clear:   hcom config gemini_args ""

Merged with launch-time cli args (launch args win on conflict).

HCOM_CODEX_ARGS - Default args passed to codex on launch

Example: hcom config codex_args "--search"
Clear:   hcom config codex_args ""

Merged with launch-time cli args (launch args win on conflict).

HCOM_OPENCODE_ARGS - Default args passed to opencode on launch

Example: hcom config opencode_args "--model o3"
Clear:   hcom config opencode_args ""

Merged with launch-time cli args (launch args win on conflict).

HCOM_CODEX_SANDBOX_MODE - Codex sandbox mode (e.g., off) (string)

HCOM_GEMINI_SYSTEM_PROMPT - System prompt for gemini on launch (string)

HCOM_CODEX_SYSTEM_PROMPT - System prompt for codex on launch (string)

HCOM_TERMINAL — where hcom opens new agent windows

Managed (open + close on kill):
  kitty          auto split/tab/window
  wezterm        auto tab/split/window
  tmux           detached sessions
  cmux           workspaces

  Variants:
    kitty: kitty-window, kitty-tab, kitty-split
    wezterm: wezterm-window, wezterm-tab, wezterm-split
    tmux: tmux-split

Other (opens window only):
  terminal.app
  iterm
  ghostty
  alacritty
  ttab
  zellij
  waveterm

Custom command (open only):
  hcom config terminal "my-terminal -e bash {script}"

Custom preset with close (~/.hcom/config.toml):
  [terminal.presets.myterm]
  open = "myterm spawn -- bash {script}"
  close = "myterm kill --id {pane_id}"
  binary = "myterm"
  pane_id_env = "MYTERM_PANE_ID"

Placeholders:
  {script}     = hcom-generated launch wrapper script path
  {pane_id}    = pane/window/workspace ID from pane_id_env; falls back to {id}
  {process_id} = HCOM_PROCESS_ID for the launched agent
  {pid}        = launched terminal process ID
  {id}         = first line of stdout captured from the open command

Set:    hcom config terminal kitty
Reset:  hcom config terminal default

HCOM_AUTO_APPROVE - Auto-approve safe hcom commands

Purpose:
  When enabled, Claude/Gemini/Codex auto-approve "safe" hcom commands
  without requiring user confirmation.

Usage:
  hcom config auto_approve 1    # Enable auto-approve
  hcom config auto_approve 0    # Disable (require approval)

Safe commands (auto-approved when enabled):
  send, start, list, events, listen, relay, config,
  transcript, archive, status, help, --help, --version

Always require approval:
  - hcom reset          (archives and clears database)
  - hcom stop           (stops instances)
  - hcom <N> claude     (launches new instances)

Values: 1, true, yes, on (enabled) | 0, false, no, off, "" (disabled)

HCOM_AUTO_SUBSCRIBE - Auto-subscribe event presets for new instances

Default: collision

Purpose:
  Comma-separated list of event subscriptions automatically added
  when an instance registers with 'hcom start'.

Usage:
  hcom config auto_subscribe "collision,created"
  hcom config auto_subscribe ""   # No auto-subscribe

Available presets:
  collision    - Alert when agents edit same file (within 30s window)
  created      - Notify when new instances join
  stopped      - Notify when instances leave
  blocked      - Notify when any instance is blocked (needs approval)

Notes:
  - Instances can add/remove subscriptions at runtime
  - See 'hcom events --help' for subscription management

HCOM_NAME_EXPORT - Export instance name to custom env var

Purpose:
  When set, launched instances will have their name exported to
  the specified environment variable. Useful for scripts that need
  to reference the current instance name.

Usage:
  hcom config name_export "MY_AGENT_NAME"   # Export to MY_AGENT_NAME
  hcom config name_export ""                 # Disable export

Example:
  # Set export variable
  hcom config name_export "HCOM_NAME"

  # Now launched instances have:
  # HCOM_NAME=luna (or whatever name was generated)

  # Scripts can use it:
  # hcom send "@$HCOM_NAME completed task"

Notes:
  - Only affects hcom-launched instances (hcom N claude/gemini/codex)
  - Variable name must be a valid shell identifier
  - Works alongside HCOM_PROCESS_ID (always set) for identity

HCOM_RELAY - MQTT broker URL

Empty = use public brokers (broker.emqx.io, broker.hivemq.com, test.mosquitto.org).
Set automatically by 'hcom relay new' (pins first working broker).

Private broker: hcom relay new --broker mqtts://host:port

HCOM_RELAY_ID - Shared UUID for relay group

Generated by 'hcom relay new'. Other devices join with 'hcom relay connect <token>'.
All devices with the same relay_id sync state via MQTT pub/sub.

HCOM_RELAY_TOKEN - Auth token for MQTT broker

Optional. Set via 'hcom relay new --password <secret>' or directly here.
Only needed if your broker requires authentication.

HCOM_RELAY_ENABLED - Enable or disable relay sync

Default: true (when relay is configured)

Usage:
  hcom config relay_enabled false    Disable relay sync
  hcom config relay_enabled true     Re-enable relay sync

Temporarily disables MQTT sync without removing relay configuration.

Per-instance config: hcom config -i <name> <key> [value]
  Keys: tag, timeout, hints, subagent_timeout
```

</details>

<details>
<summary>Scripts</summary>

```
# Creating Custom Scripts

## Location

  User scripts:    ~/.hcom/scripts/
  File types:      *.sh (bash), *.py (python3)

User scripts shadow bundled scripts with the same name.
Scripts are discovered automatically — drop a file and run `hcom run <name>`.
Add a description comment on line 2 (after shebang) — it shows in `hcom run` listings.

## Shell Script Template

  #!/usr/bin/env bash
  # Brief description shown in hcom run list.
  set -euo pipefail

  name_flag=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      -h|--help) echo "Usage: hcom run myscript [OPTIONS]"; exit 0 ;;
      --name) name_flag="$2"; shift 2 ;;
      --target) target="$2"; shift 2 ;;
      *) shift ;;
    esac
  done

  name_arg=""
  [[ -n "$name_flag" ]] && name_arg="--name $name_flag"

  # Your logic here
  hcom send "@${target}" $name_arg --intent request -- "Do the task"

## Identity Handling

hcom passes --name to scripts automatically. Always parse and forward it:

  name_arg=""
  [[ -n "$name_flag" ]] && name_arg="--name $name_flag"
  hcom send @target $name_arg -- "message"
  hcom list self --json $name_arg

## Launching & Cleaning Up Agents

Launch output includes "Names: <name>" — parse to track spawned agents:

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

  launch_out=$(hcom 1 claude --tag worker --go --headless 2>&1)
  track_launch "$launch_out"

## Reference Examples

View source of any bundled or user script:

  hcom run <name> --source

See `hcom run docs --cli` for full CLI command reference.
Available scripts:
  hcom run confess --source
  hcom run debate --source
  hcom run fatcow --source
```

</details>

<details>
<summary>Build</summary>


```bash
# Prerequisites: Rust 1.86+

git clone https://github.com/aannoo/hcom.git
cd hcom
cargo test
cargo install --path . --force
```

Worktrees use the installed `hcom` as a bootstrap binary. Build inside the
worktree, then point `HCOM_DEV_ROOT` at it:

```bash
cd /path/to/worktree
cargo build --release
HCOM_DEV_ROOT=/path/to/worktree hcom list
```

</details>


---

## Contributing

Issues and PRs welcome. The codebase is Rust — `cargo test && cargo install --path . --force` is the standard local workflow.

---

## License

[MIT](LICENSE)
