# hcom

[![PyPI](https://img.shields.io/pypi/v/hcom)](https://pypi.org/project/hcom/)
[![Python 3.11+](https://img.shields.io/badge/python-3.11+-blue.svg)](https://www.python.org/downloads/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

AI agents running in separate terminals are isolated from each other. Context doesn't transfer, decisions get repeated, file edits collide.

hcom connects them. When one agent edits a file, runs a command, or sends a message, the others find out immediately.

![demo](https://raw.githubusercontent.com/aannoo/hcom/refs/heads/assets/screencapture-new-new.gif)

---

## Quickstart

```bash
pip install hcom
```

Run agents with `hcom` in front:

```bash
hcom claude
hcom gemini
hcom codex
```

Prompt:

> send a message to claude

Open the TUI:

```bash
hcom
```

---

## What you can do

Tell any agent:

> when codex goes idle, send it the next task, and get claude to review the previous

> watch gemini's file edits, review each and send feedback if any bugs

> fork yourself to investigate the bug and report back

> find which agent worked on terminal_id code, resume them and ask why it sucks

Messages arrive mid-turn or wake idle agents immediately.

Collision detection is on by default. If 2 agents edit the same file within 20s, both get notified.

Agents get unique names. You can also refer to them by tool, terminal, branch, cwd, or set custom: *"tag yourself reviewer"*.

<details><summary>What agents can do</summary>

* Message each other (with intents, threads, broadcast, @mentions)

* Read each other's transcripts (with ranges and detail levels)

* View other's terminal screens and inject text / enter for permission approval

* Query event history (file edits, commands, status, lifecycle events)

* Subscribe and react to each other's event activity

* Spawn, fork, resume other agents into new terminal windows/panes/tabs. and kill them.

* Build bundles of context (files, transcript, events) for handoffs

</details>




## Workflow scripts

Included examples of multi-agent setups built with the Python API.

**`hcom run confess`** - agent (or background clone) writes an honesty self-eval, a spawned calibrator reads the target's transcript independently, judge compares both reports, sends back a verdict via hcom message.

**`hcom run debate`** - judge spawns then sets up a debate with existing agents - coordinates rounds in a shared thread where all agents see each other's arguments, with shared context of workspace files and transcripts.

**`hcom run fatcow`** — headless agent reads every file in a path, subscribes to file edit events to stay current, answers other agents on demand.

Create your own by prompting:

> "read `hcom run docs` then make a script that does X"


---

## Tools

**Claude Code, Gemini CLI, Codex** — automatic message delivery, transcript/terminal screen etc.

**Any AI tool that can run shell commands** - send and receive messages. Tell it "run `hcom start`"


<details><summary>Ping from any process</summary>


```bash
hcom send <message> --from bot-name
```
</details>

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

Spawning works with any terminal emulator. Run `hcom config terminal --info` to set up.

Closing/killing works with kitty, wezterm (auto open/close panes), and tmux (background).

```bash
hcom config terminal kitty    # set
```

---

## Cross-device

Connect agents across machines through a private HuggingFace Space (free):

```bash
# local
hcom relay hf <HF_TOKEN>

# remote/cloud
pip install hcom && hcom relay hf <HF_TOKEN> && hcom start -h
```

---

## What gets installed

Hooks go into `~/` (or `HCOM_DIR`) on launch or `hcom start`. If you aren't using hcom, the hooks do nothing.

```bash
hcom hooks remove                  # safely remove only hcom hooks
hcom status                        # install status
```

```bash
HCOM_DIR=$PWD/.hcom                # for sandbox or project local
```

---

## Reference

<details>
<summary>CLI</summary>

```
# hcom CLI Reference

hcom (hook-comms) v0.6.18 - multi-agent communication

Usage:
  hcom                                  TUI dashboard
  hcom <N> claude|gemini|codex [args]   Launch agents (args forwarded to tool)
  hcom <command>                        Run command

Commands:
  send         Send message to your buddies
  listen       Block until message or event arrives
  list         Show agents, status, unread counts
  events       Query event stream, manage subscriptions
  bundle       Structured context packages for handoffs
  transcript   Read another agent's conversation
  start        Connect to hcom (run inside any AI tool)
  stop         Disconnect from hcom
  kill         Terminate agent + close terminal pane
  config       Get/set global and per-agent settings
  run          Execute workflow scripts
  relay        Cross-device communication
  archive      Query past hcom sessions
  reset        Archive and clear database
  hooks        Add or remove hooks
  status       Installation and diagnostics
  term         View/inject into agent PTY screens

Identity:
  1. Run hcom start to get a name
  2. Use --name <name> on all hcom commands

Run 'hcom <command> --help' for details.



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

    Core:                          
    --agent NAME                   Agent name
    --type TYPE                    message | status | life
    --status VAL                   listening | active | blocked
    --context PATTERN              tool:Bash | deliver:X (supports * wildcard)
    --action VAL                   created | started | ready | stopped | batch_launched

    Command / file:                
    --cmd PATTERN                  Shell command (contains, ^prefix, $suffix, =exact, *glob)
    --file PATH                    File write (*.py for glob, file.py for contains)
    --collision                    Two agents edit same file within 20s

    Message:                       
    --from NAME                    Sender
    --mention NAME                 @mention target
    --intent VAL                   request | inform | ack
    --thread NAME                  Thread name

    Time:                          
    --after TIME                   After timestamp (ISO-8601)
    --before TIME                  Before timestamp (ISO-8601)


Shortcuts:
    --idle NAME                    --agent NAME --status listening
    --blocked NAME                 --agent NAME --status blocked


Subscribe (hcom notification when event matches):
    events sub                     List subscriptions
    events sub [filters]           Create subscription with filter flags
      --once                       Auto-remove after first match
      --for <name>                 Subscribe for another agent
    events unsub <id>              Remove subscription


Examples:
    events --agent peso --status listening 
    events --cmd git --agent peso  


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

## list

Usage:
  hcom list                       All alive agents, read receipts
    -v                             Verbose (directory, session, etc)
    --json                         Verbose JSON (NDJSON, one per line)

  hcom list [self|<name>]         Single agent details
    [field]                        Print specific field (status, directory, session_id, ...)
    --json                         Output as JSON
    --sh                           Shell exports: eval "$(hcom list self --sh)"

  hcom list --stopped [name]      Stopped instances (from events)
    --all                          All stopped (default: last 20)


Status icons:
  ▶  active      processing, reads messages very soon
  ◉  listening   idle, reads messages in <1s
  ■  blocked     needs human approval
  ○  inactive    dead or stale
  ◦  unknown     neutral


Tool labels:
  [CLAUDE] [GEMINI] [CODEX]  hcom-launched (PTY + hooks)
  [claude] [gemini] [codex]  vanilla (hooks only)
  [AD-HOC]                   manual polling

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


  Inside a sandbox? Prefix all hcom commands with: HCOM_DIR=$PWD/.hcom

## kill

Usage:
  hcom kill <name>                Kill process (+ close terminal pane)
  hcom kill tag:<name>            Kill all with tag
  hcom kill all                   Kill all with tracked PIDs


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

## reset

Usage:
  hcom reset                      Archive conversation, clear database
  hcom reset all                  Stop all + clear db + remove hooks + reset config


Sandbox / local mode:
  If you can't write to ~/.hcom, set:
    export HCOM_DIR="$PWD/.hcom"
  Hooks install under $PWD (.claude/.gemini/.codex), state in $HCOM_DIR

  To remove local setup:
    hcom hooks remove && rm -rf "$HCOM_DIR"

  Explicit location:
    export HCOM_DIR=/your/path/.hcom


## config

Usage:
  hcom config                     Show all config values
  hcom config <key>               Get single value
  hcom config <key> <value>       Set value
  hcom config <key> --info        Detailed help for a setting (presets, examples)
    --json                         JSON output
    --edit                         Open config in $EDITOR
    --reset                        Reset config to defaults

  hcom config terminal --setup    Configure kitty remote control


Per-agent runtime config:
  hcom config -i <name>           Show agent config
  hcom config -i <name> <key>     Get value
  hcom config -i <name> <key> <val> Set value
    -i self                        Current agent
    keys: tag, timeout, hints, subagent_timeout 


Global settings:
    HCOM_TAG                       Group tag (agents become tag-*)
    HCOM_TERMINAL                  default | <preset> | "cmd {script}"
    HCOM_HINTS                     Text appended to all messages agent receives
    HCOM_NOTES                     One-time notes appended to bootstrap
    HCOM_SUBAGENT_TIMEOUT          Subagent keep-alive seconds after task
    HCOM_CLAUDE_ARGS               Default claude args (e.g. "--model opus")
    HCOM_GEMINI_ARGS               Default gemini args
    HCOM_CODEX_ARGS                Default codex args
    HCOM_RELAY                     Relay server URL (set by 'hcom relay hf')
    HCOM_RELAY_TOKEN               HuggingFace token (set by 'hcom relay hf')
    HCOM_AUTO_APPROVE              Auto-approve safe hcom commands (1|0)
    HCOM_AUTO_SUBSCRIBE            Auto-subscribe presets (e.g. "collision")
    HCOM_NAME_EXPORT               Export agent name to custom env var

  Non-HCOM_* vars in config.env pass through to claude/gemini/codex
  e.g. ANTHROPIC_MODEL=opus


Precedence: HCOM defaults < config.env < shell env vars
  Each resolves independently

  HCOM_DIR: per project/sandbox — must be set in shell (see 'hcom reset --help')

## relay

Usage:
  hcom relay                      Show relay status
  hcom relay on                   Enable cross-device communication
  hcom relay off                  Disable cross-device communication
  hcom relay pull                 Force sync now
  hcom relay hf [token]           Setup HuggingFace Space relay
    --update                       Update existing Space

  Finds or duplicates a private, free HF Space to your account.
  Provide HF_TOKEN or run 'huggingface-cli login' first.
  Remote agents appear with :SUFFIX (e.g. luna:BOXE).

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
    --agent TYPE                   Filter: claude | gemini | codex
    --json                         JSON output

  Tip: Reference ranges in messages instead of copying:
  "read my transcript range 7-10 --full"

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

## run

Usage:
  hcom run                        List available workflow/launch scripts and more info
  hcom run <name> [args]          Execute script
  hcom run <name> --help          Script options
  hcom run docs                   Python API + CLI reference + examples

  Docs sections:
    hcom run docs --cli            CLI reference only
    hcom run docs --config         Config settings only
    hcom run docs --api            Python API + scripts guide

  User scripts: ~/.hcom/scripts/

## claude

Usage:
  hcom [N] claude [args...]       Launch N Claude agents (default N=1)

    hcom claude                    Runs in current terminal
    hcom N claude (N>1)            Opens new terminal windows
    hcom N claude "initial prompt" Initial prompt (positional)
    hcom 3 claude -p "prompt"      3 headless in background
    HCOM_TAG=api hcom 2 claude     Group tag (creates api-*)
    hcom 1 claude --agent <name>   .claude/agents/<name>.md
    hcom 1 claude --system-prompt "text" System prompt
    HCOM_TERMINAL=tmux hcom 1 claude detached tmux launch


Environment:
    HCOM_TAG                       Group/label tag (agents become tag-*)
    HCOM_TERMINAL                  default | <preset> | "cmd {script}"
    HCOM_CLAUDE_ARGS               Default args (merged with CLI)
    HCOM_HINTS                     Appended to messages received
    HCOM_NOTES                     One-time bootstrap notes
    HCOM_SUBAGENT_TIMEOUT          Seconds subagents are keep-alive after task


Resume / Fork:
    hcom r <name>                  Resume stopped agent by name
    hcom f <name>                  Fork agent session (active or stopped)

  Run "claude --help" for claude options.
  Run "hcom config terminal --info" for terminal presets.

## gemini

Usage:
  hcom [N] gemini [args...]       Launch N Gemini agents (default N=1)

    hcom gemini                    Runs in current terminal
    hcom N gemini (N>1)            Opens new terminal windows
    hcom N gemini -i "initial prompt" Initial prompt (-i flag required)
    hcom N gemini --yolo           Flags forwarded to gemini
    HCOM_TAG=api hcom 2 gemini     Group tag (creates api-*)
    HCOM_TERMINAL=tmux hcom 1 gemini detached tmux launch


Environment:
    HCOM_TAG                       Group tag (agents become tag-*)
    HCOM_TERMINAL                  default | <preset> | "cmd {script}"
    HCOM_GEMINI_ARGS               Default args (merged with CLI)
    HCOM_HINTS                     Appended to messages received
    HCOM_NOTES                     One-time bootstrap notes
    HCOM_GEMINI_SYSTEM_PROMPT      Use this for system prompt


Resume:
    hcom r <name>                  Resume stopped agent by name
  Gemini does not support session forking (hcom f).

  Run "gemini --help" for Gemini CLI options.
  Run "hcom config terminal --info" for terminal presets.

## codex

Usage:
  hcom [N] codex [args...]        Launch N Codex agents (default N=1)

    hcom codex                     Runs in current terminal
    hcom N codex (N>1)             Opens new terminal windows
    hcom N codex "initial prompt"  Initial prompt (positional)
    hcom codex --sandbox danger-full-access Flags forwarded to codex
    HCOM_TAG=api hcom 2 codex      Group tag (creates api-*)
    HCOM_TERMINAL=tmux hcom 1 codex detached tmux launch


Environment:
    HCOM_TAG                       Group tag (agents become tag-*)
    HCOM_TERMINAL                  default | <preset> | "cmd {script}"
    HCOM_CODEX_ARGS                Default args (merged with CLI)
    HCOM_HINTS                     Appended to messages received
    HCOM_NOTES                     One-time bootstrap notes
    HCOM_CODEX_SYSTEM_PROMPT       System prompt (env var or config)


Resume / Fork:
    hcom r <name>                  Resume stopped agent by name
    hcom f <name>                  Fork agent session (active or stopped)

  Run "codex --help" for Codex options.
  Run "hcom config terminal --info" for terminal presets.

## status

Usage:
  hcom status                     Installation status and diagnostics
  hcom status --logs              Include recent errors and warnings
  hcom status --json              Machine-readable output

## hooks

Usage:
  hcom hooks                      Show hook status
  hcom hooks status               Same as above
  hcom hooks add [tool]           Add hooks (claude | gemini | codex | all)
  hcom hooks remove [tool]        Remove hooks (claude | gemini | codex | all)

  Hooks enable automatic message delivery and status tracking.
  Without hooks, use ad-hoc mode (run hcom start inside any AI tool).
  Restart the tool after adding hooks to activate.
  Remove cleans both global (~/) and HCOM_DIR-local if set.

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
```

</details>

<details>
<summary>Config</summary>

```
# Config Settings Reference

Config is stored in ~/.hcom/config.env (or $HCOM_DIR/config.env).

Commands:
  hcom config                 Show all values
  hcom config <key> <val>     Set value
  hcom config <key> --info    Detailed help for a setting
  hcom config --edit          Open in $EDITOR

Precedence: defaults < config.env < shell environment variables

## HCOM_TAG

HCOM_TAG - Group tag for launched instances

Current value: Use 'hcom config tag' to see current value

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

## HCOM_TERMINAL

HCOM_TERMINAL — where hcom opens new agent windows

Current: default (Terminal.app)

Managed (open + close on kill):
  kitty          auto split/tab/window
  wezterm        auto tab/split/window
  tmux           detached sessions

  Variants:
    kitty: kitty-window, kitty-tab, kitty-split
    wezterm: wezterm-window, wezterm-tab, wezterm-split
    tmux: tmux-split

Other (opens window only):
  Terminal.app
  iTerm
  Ghostty
  alacritty
  ttab               npm install -g ttab

Custom command (open only):
  hcom config terminal "my-terminal -e bash {script}"

Custom preset with close (~/.hcom/settings.toml):
  [terminal.myterm]
  open = "myterm spawn -- bash {script}"
  close = "myterm kill --id {id}"
  binary = "myterm"

  {id} = stdout from the open command.
  {pid} and {process_id} also available.

Set:    hcom config terminal kitty
Reset:  hcom config terminal default

## HCOM_HINTS

HCOM_HINTS - Text injected with all messages

Current value: Use 'hcom config hints' to see current value

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

## HCOM_NOTES

HCOM_NOTES - One-time notes appended to bootstrap

  Custom text added to agent system context at startup.
  Unlike HCOM_HINTS (per-message), this is injected once and does not repeat.

Usage:
  hcom config notes "Always check hcom list before spawning new agents"
  hcom config notes ""                            # Clear
  HCOM_NOTES="tips" hcom 1 claude                 # Per-launch override

  Changing after launch has no effect (bootstrap already delivered).

## HCOM_SUBAGENT_TIMEOUT

HCOM_SUBAGENT_TIMEOUT - Timeout for Claude subagents (seconds)

Current value: Use 'hcom config subagent_timeout' to see current value
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

## HCOM_CLAUDE_ARGS

HCOM_CLAUDE_ARGS - Default args passed to claude on launch

Example: hcom config claude_args "--model opus"
Clear:   hcom config claude_args ""

Merged with launch-time cli args (launch args win on conflict).

## HCOM_GEMINI_ARGS

HCOM_GEMINI_ARGS - Default args passed to gemini on launch

Example: hcom config gemini_args "--model gemini-2.5-flash"
Clear:   hcom config gemini_args ""

Merged with launch-time cli args (launch args win on conflict).

## HCOM_CODEX_ARGS

HCOM_CODEX_ARGS - Default args passed to codex on launch

Example: hcom config codex_args "--search"
Clear:   hcom config codex_args ""

Merged with launch-time cli args (launch args win on conflict).

## HCOM_RELAY

HCOM_RELAY - Relay server URL

Set automatically by 'hcom relay hf'.

Custom server: implement POST /push/{device_id}, GET /poll, GET /version
See: https://huggingface.co/spaces/aannoo/hcom-relay/blob/main/app.py

## HCOM_RELAY_TOKEN

HCOM_RELAY_TOKEN - HuggingFace token for private Space auth
Set automatically by 'hcom relay hf'

Or optional authentication token for custom server.

## HCOM_AUTO_APPROVE

HCOM_AUTO_APPROVE - Auto-approve safe hcom commands

Current value: Use 'hcom config auto_approve' to see current value

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

## HCOM_AUTO_SUBSCRIBE

HCOM_AUTO_SUBSCRIBE - Auto-subscribe event presets for new instances

Current value: Use 'hcom config auto_subscribe' to see current value
Default: collision

Purpose:
  Comma-separated list of event subscriptions automatically added
  when an instance registers with 'hcom start'.

Usage:
  hcom config auto_subscribe "collision,created"
  hcom config auto_subscribe ""   # No auto-subscribe

Available presets:
  collision    - Alert when agents edit same file (within 20s window)
  created      - Notify when new instances join
  stopped      - Notify when instances leave
  blocked      - Notify when any instance is blocked (needs approval)

Notes:
  - Instances can add/remove subscriptions at runtime
  - See 'hcom events --help' for subscription management

## HCOM_NAME_EXPORT

HCOM_NAME_EXPORT - Export instance name to custom env var

Current value: Use 'hcom config name_export' to see current value

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
```

</details>

<details>
<summary>Python API</summary>

```
# hcom Python API Reference

## hcom.session()

Get an identity-bound session for hcom operations.

Args:
    name: Instance name. Auto-detects from environment if None.
    external: If True, creates external sender (no instance required).

Returns:
    Session object with messaging and event methods.

Raises:
    HcomError: If name required but not provided or not found.

Examples:
    s = hcom.session()                    # auto-detect
    s = hcom.session(name="luna")        # explicit instance
    s = hcom.session(name="ci", external=True)  # external sender

## hcom.instances()

List active instances or get one by name.

Args:
    name: Specific instance name, or None for all.

Returns:
    If name: dict with keys name, session_id, status, directory, parent_name, tool
    If None: list of such dicts

Raises:
    HcomError: If name specified but not found.

Examples:
    all_instances = hcom.instances()
    nova = hcom.instances(name="nova")

## hcom.launch()

Launch AI tool instances.

Two calling modes:

Single launch:
    hcom.launch(3, tag="worker", prompt="do task")
    hcom.launch(1, tool="gemini", prompt="review code")

Group launch (heterogeneous agents, shared batch):
    hcom.launch([
        {"tag": "confessor", "prompt": "...", "background": True},
        {"tool": "gemini", "tag": "calibrator", "prompt": "..."},
        {"tool": "gemini", "tag": "judge", "prompt": "..."},
    ])

Args:
    count: Number of instances (int) or list of spec dicts.
    tool: One of 'claude', 'gemini', 'codex' (single mode).
    tag: Group tag (single mode).
    prompt: Initial prompt (single mode).
    system_prompt: System prompt override (single mode).
    background: If True, run headless (single mode).
    claude_args: Additional Claude CLI args (single mode).
    resume: Instance name or session ID to resume from (single mode).
        If an instance name (e.g., "luna"), loads snapshot automatically:
        sets tool, tag, cwd, background, system_prompt, and restores
        message delivery cursor. If a raw session ID, passes through as-is.
    fork: If True with resume, fork instead of continue (single mode).
        Fork creates a new instance from the same session history.
    tool_args: Additional tool-specific args (single mode).
    cwd: Working directory (single mode).
    wait: If True (default), block until all instances are ready or timeout.
    timeout: Max seconds to wait when wait=True.
    batch_id: Share a batch ID across multiple launch() calls for wait-for-all.
    name: Explicit instance name (single mode). Used for resume to reuse
        the stopped instance's name. Requires count=1 and name must not
        be in use by an active instance.

Spec dict keys (group mode):
    count (int): Number of instances, default 1.
    tool (str): 'claude', 'gemini', or 'codex', default 'claude'.
    tag, prompt, system_prompt, background, claude_args,
    resume, fork, tool_args, cwd: Same as single mode args.

Returns:
    Single mode: Dict with tool, batch_id, launched, failed, background,
        log_files, handles, errors, launch_status (if wait=True).
    Group mode: Dict with batch_id, results (list of per-spec dicts),
        total_launched, total_failed, launch_status (if wait=True).

Raises:
    HcomError: On invalid tool, hook setup failure, or launch failure.

## hcom.bundle()

Manage bundles for context handoff and review workflows.

Bundles package conversation transcript ranges, event IDs, and file paths
into referenceable context units for handoffs between agents.

Args:
    action: One of 'list', 'show', 'create', 'chain'.
    title: Title for new bundle.
    description: Description for new bundle.
    events: List of event IDs/ranges for new bundle (e.g., ["123-125", "130"]).
    files: List of file paths for new bundle.
    transcript: List of transcript ranges for new bundle (e.g., ["5-10", "15"]).
    extends: Parent bundle ID for chaining related work.
    data: Full bundle dict (alternative to separate fields).
    bundle_id: ID for show/chain actions.
    last: Limit for list action.

Returns:
    list (for 'list', 'chain'): List of bundle dicts.
    dict (for 'show'): Bundle details.
    str (for 'create'): New bundle ID.

Examples:
    # Create a bundle
    bundle_id = hcom.bundle("create",
        title="Code review: auth module",
        description="Implementation complete, ready for review",
        events=["123-125", "130"],
        files=["auth.py", "tests/test_auth.py"],
        transcript=["10-15"]
    )

    # List recent bundles
    bundles = hcom.bundle("list", last=10)

    # Get bundle details
    details = hcom.bundle("show", bundle_id="abc123")

    # Get bundle chain (all related bundles)
    chain = hcom.bundle("chain", bundle_id="abc123")

## Session

Identity-bound session for hcom operations.

Provides messaging, events, and transcript access tied to a specific
instance identity. Data is fetched fresh on each call (no caching).

Create via hcom.session():
    s = hcom.session()                    # auto-detect
    s = hcom.session(name="luna")        # explicit
    s = hcom.session(name="bot", external=True)  # external sender

### Session.name

Instance name (e.g., 'luna' or 'worker-0').

### Session.info

Fresh instance info from database.

    Returns:
        Dict with keys:
            name (str): Full instance name (may include tag prefix).
            session_id (str): Claude session ID for transcript binding.
            connected (bool): True if instance exists in DB, False if external.
            directory (str): Working directory path.
            status (str): Current status ('active', 'listening', 'inactive').
            transcript_path (str): Path to transcript file.
            parent_name (str): Parent instance name (for subagents).
            tool (str): Tool type ('claude', 'gemini', 'codex').

    Raises:
        HcomError: If instance no longer exists.

### Session.send

Send message to instances.

    Args:
        message: Message text. Use @name or @prefix- for targeting.
        to: Target name (auto-prepends @name if not in message).
        intent: One of 'request', 'inform', 'ack'.
        reply_to: Event ID to reply to (required for intent='ack').
        thread: Thread name for grouping related messages.
        bundle: Bundle dict to create and attach. If provided, creates bundle event
            and appends bundle summary to message.

    Returns:
        List of instance names that received the message.

    Examples:
        s.send("@nova hello")
        s.send("@worker- start task", thread="batch-1", intent="request")
        s.send("received", to="luna", intent="ack", reply_to="42")

        # With bundle
        s.send("@reviewer check this", bundle={
            "title": "Code review",
            "description": "Auth module complete",
            "refs": {
                "events": ["123-125"],
                "files": ["auth.py"],
                "transcript": ["10-15"]
            }
        })

### Session.messages

Get messages for this instance.

    Args:
        unread: If True, only messages delivered to this instance (mentions or
            broadcasts). If False, returns all messages in the system.
        last: Maximum number of messages to return (most recent first).

    Returns:
        List of dicts with keys:
            ts (str): ISO timestamp when message was sent.
            from (str): Sender's display name.
            text (str): Message text content.
            mentions (list[str]): Instance names mentioned in message.
            delivered_to (list[str]): Instance names message was delivered to.
            intent (str, optional): Message intent ('request', 'inform', 'ack').
            thread (str, optional): Thread name for grouping messages.
            reply_to (int, optional): Event ID this message replies to.

### Session.events

Query the event stream.

    Filter kwargs mirror CLI flags (--agent, --file, --after, etc.).
    Same composition: different filters AND together, multiple values OR.
    Raw sql is ANDed with structured filters when both provided.

    Args:
        sql: Raw SQL WHERE clause (ANDed with structured filters).
        params: Parameters for SQL placeholders (?).
        last: Maximum events to return.
        agent: Instance name(s).
        type: Event type (message, status, life).
        file: File path pattern(s) — glob (*.py) or contains match.
        after: ISO-8601 timestamp lower bound.
        before: ISO-8601 timestamp upper bound.
        status: Status value (listening, active, blocked).
        context: Status context pattern (tool:Bash, deliver:*).
        action: Life action (created, ready, stopped).
        sender: Message sender name (maps to --from).
        intent: Message intent (request, inform, ack).
        thread: Message thread name.

    Returns:
        List of dicts with keys: ts, type, instance, data

    SQL fields (for raw sql= queries against events_v):
        Common: id, timestamp, type, instance
        Message: msg_from, msg_text, msg_thread, msg_intent,
                 msg_reply_to, msg_mentions, msg_delivered_to, msg_bundle_id
        Status: status_val, status_context, status_detail
        Lifecycle: life_action, life_by, life_batch_id

    Examples:
        s.events(file="src/core/*", after="2026-01-01T00:00:00")
        s.events(agent="nova", type="message")
        s.events(sender="luna", intent="request", last=50)
        s.events(sql="msg_from=?", params=["nova"])  # raw SQL still works

### Session.wait

Block until an event matches the SQL condition.

    Args:
        sql: SQL WHERE clause to match.
        params: Parameters for SQL placeholders (?).
        timeout: Seconds to wait before returning None.

    Returns:
        Matching event dict, or None if timeout.

    Examples:
        event = s.wait("msg_from='nova'", timeout=60)
        event = s.wait("msg_thread=?", params=["task-1"], timeout=120)

### Session.subscribe

Create a push subscription for events.

    When matching events occur, a notification is sent via hcom.

    Args:
        sql: SQL WHERE clause to match events.
        params: Parameters for SQL placeholders (?).
        once: If True, subscription auto-removes after first match.

    Returns:
        Subscription ID (e.g., 'sub-a1b2').

    Raises:
        HcomError: If called from external session (can't receive notifications).

    Examples:
        sub_id = s.subscribe("msg_thread='task-1'")
        sub_id = s.subscribe("msg_from=?", params=["nova"], once=True)

### Session.subscriptions

List all active event subscriptions.

    Returns:
        List of dicts with keys: id, sql, caller, once

### Session.unsubscribe

Remove an event subscription.

    Args:
        sub_id: Subscription ID (with or without 'sub-' prefix).

    Returns:
        True if removed, False if not found.

### Session.transcript

Get conversation transcript for an instance or timeline across all instances.

    Args:
        agent: Instance name, or "timeline" for timeline mode (all instances).
        last: Number of recent exchanges to return.
        full: If True, include truncated tool output (use detailed for full output).
        range: Exchange range like "5-10" (1-indexed, inclusive). Only valid for specific agent.
        detailed: If True, include full tool calls, results, file edits, errors.

    Returns:
        If agent is name: List of exchange dicts with keys: user, assistant, position, timestamp
        If agent is "timeline": List of entry dicts with keys: instance, position,
            user, action, timestamp, files, command

    Examples:
        s.transcript("nova")                     # nova's transcript
        s.transcript("nova", last=5)             # nova's last 5
        s.transcript("nova", range="1-10")       # nova's exchanges 1-10
        s.transcript("timeline", last=20)        # timeline across all agents
        s.transcript("timeline", detailed=True)  # detailed timeline

### Session.stop

Stop this instance's hcom participation.

    The instance will no longer receive messages or appear in listings.

## hcom.HcomError

Exception raised for hcom errors.

# Creating Custom Scripts

## Location

User scripts (shadow bundled by name):
  ~/.hcom/scripts

Bundled scripts (reference examples):
  <hcom-package>/scripts/bundled

File types:
  *.py   Python scripts (executable with python3)
  *.sh   Shell scripts (executable with bash)

## Script Structure Template

# --- python ---
#!/usr/bin/env python3
"""Brief one-line description shown in hcom run list."""
import argparse
import sys
import hcom

def main():
parser = argparse.ArgumentParser(description="...")
parser.add_argument('--target', help='target instance')
parser.add_argument('--name', help='instance identity (optional)')

args = parser.parse_args()

s = hcom.session(name=args.name) if args.name else hcom.session()
s.send("@target hello")

return 0

if __name__ == '__main__':
sys.exit(main())
# ---
## Common Workflow Patterns

Launch agents:
  instances = hcom.launch(count=3, tool='claude', tag='worker',
                      prompt='your task here')

Send messages:
  s = hcom.session()
  s.send("@luna check this")               # to specific instance
  s.send("@group- broadcast to group")    # to all in group
  s.send("message", intent="request")     # with envelope

Subscribe to events:
  s.subscribe("instance='luna' AND type='status'")
  s.wait()  # block until event matches

Read transcripts:
  exchanges = s.transcript('luna', last=5, detailed=True)    # specific agent
  timeline = s.transcript('timeline', last=20)               # all agents (timeline)

Check messages:
  for msg in s.messages():
  print(msg['text'])

## Identity Handling

Auto-detect (when run from hcom instance):
  s = hcom.session()

Explicit (for testing or specific workflows):
  s = hcom.session(name="watcher-luna")

External (script not run by an instance):
  s = hcom.session(name="ci-bot", external=True)

Support --name flag in your script:
  parser.add_argument('--name', help='instance identity')
  s = hcom.session(name=args.name) if args.name else hcom.session()

## Reference Examples

View bundled script sources as working examples:
  hcom run clone --source        # Spawn clone for a task, result sent back automatically via hcom. (1 fork)
  hcom run confess --source      # Honesty self-evaluation based on OpenAI's confessions paper. (3 agents)
  hcom run debate --source       # PRO/CON debaters (fresh or existing agents) + judge evaluate a topic in shared hcom thread. (2+ agents)
  hcom run ensemble --source     # Ensemble Refinement - multiple agents implement, see each other's work, refine iterativly with a judge. (3-6 agents)
  hcom run fatcow --source       # Fat cow - a fat agent that deeply reads a module and answers questions on demand.
  hcom run glue --source         # Background glue that watches transcript timeline and connects dots across agents. (1 agent)
  hcom run watcher --source      # Background code reviewer that subscribes to activity, sends review back via hcom (1 agent)

## Best Practices

- Use hcom API for all hcom operations (not CLI commands in subprocess)
- Support --name flag for identity (allows external callers)
- Scripts can call other scripts: hcom.launch() or subprocess to hcom run

# Examples

View workflow script sources:

  hcom run clone --source
  hcom run confess --source
  hcom run debate --source
  hcom run ensemble --source
  hcom run fatcow --source
  hcom run glue --source
  hcom run watcher --source
```

</details>

<details>
<summary>Build</summary>


```bash
# Prerequisites: Rust 1.85+, Python 3.11+

git clone https://github.com/aannoo/hcom.git
cd hcom
pip install -e . --no-build-isolation

# Put rust binary on PATH
ln -sf $(pwd)/src/native/target/release/hcom ~/.local/bin/hcom

# Build (+ copy binary + restart daemon)
./build.sh

# Testing
pytest test/
cargo -C src/native test

# platform wheel in target/wheels/
pip install maturin
maturin build --release
```

</details>


---
## License

MIT
