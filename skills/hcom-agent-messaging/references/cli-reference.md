# hcom CLI Command Reference

Complete reference for all hcom commands, flags, and usage patterns.

## Global Flags

These flags can be used with any command:

| Flag | Description |
|------|-------------|
| `--name <NAME>` | Specify identity (instance name, tag-name, or agent_id) |
| `--go` | Skip confirmation prompts (required in scripts) |
| `--help` | Show help for command |

## Launching Agents

### `hcom [count] <tool> [flags]`

Launch one or more AI coding agents.

**Tools:** `claude`, `codex`, `gemini`, `opencode`

| Flag | Description | Default |
|------|-------------|---------|
| `--tag <TAG>` | Group tag for routing (`@tag-` prefix) | none |
| `--headless` | Run as detached background process | false |
| `--go` | Skip confirmation prompt | false |
| `--hcom-prompt <TEXT>` | Initial task/prompt for the agent | none |
| `--hcom-system-prompt <TEXT>` | System prompt (personality/role) | none |
| `--terminal <PRESET>` | Terminal preset (tmux, kitty, wezterm, here, etc.) | auto-detect |
| `--batch-id <ID>` | Group related launches for coordinated init | none |

**Tool-specific flags (forwarded):**
- Claude: `--model`, `--system-prompt`, `--resume`, `--fork-session`, `-p` (print/headless), `--verbose`, `--continue`, `--ide`
- Codex: `--model`, `-c` (config), `--sandbox`, `--full-auto`, `--add-dir`, `exec`, `resume`, `fork`
- Gemini: `--model`, `-r`/`--resume`, `--yolo`, `--sandbox`, `-e`/`--extensions`
- OpenCode: standard flags

**Examples:**
```bash
hcom 1 claude --tag worker --go --headless --hcom-prompt "Do X"
hcom 3 claude --tag team --go --headless --hcom-prompt "Answer question"
hcom 1 codex --tag eng --go --headless --hcom-prompt "Implement spec"
hcom claude                          # Interactive Claude in new terminal
hcom codex --terminal tmux           # Codex in tmux split
```

**Output:**
```
Names: luna        # Single agent
Names: luna nemo bali   # Batch launch (space-separated)
```

## Messaging

### `hcom send [@target...] [flags] -- <message>`

Send a message to one or more agents.

| Flag | Description |
|------|-------------|
| `@name` | Direct target (agent name) |
| `@tag-` | All agents with this tag prefix |
| `@name:DEVICE` | Remote agent on specific device |
| `--intent <TYPE>` | request, inform, or ack |
| `--thread <ID>` | Thread for conversation isolation |
| `--reply-to <EVENT_ID>` | Reference a specific event |
| `--from <NAME>` | Send as external identity (not an agent) |
| `--file <PATH>` | Send file content as message |
| `--base64 <STRING>` | Send base64-encoded content |
| `--stdin` | Read message from stdin |

**Message delimiters:** Use `--` before message text to prevent flag parsing:
```bash
hcom send @luna -- "this is the message"
hcom send @luna --intent request -- "review this code"
```

**Examples:**
```bash
hcom send @worker- --thread review-1 --intent request -- "please review"
hcom send @luna @nova -- "sync up"
hcom send -- "broadcast to all"
hcom send @luna --from "github-ci" -- "PR merged"
cat report.md | hcom send @reviewer --stdin
hcom send @luna --file /tmp/output.log
```

## Listing Agents

### `hcom list [flags]`

Show active, stopped, and remote agents.

| Flag | Description |
|------|-------------|
| `-v` | Verbose output (includes timestamps, PIDs, directories) |
| `--json` | JSON output |
| `--names` | Names only (space-separated) |
| `--format <TEMPLATE>` | Custom format (e.g., `'{name} {status}'`) |
| `--all` | Include all stopped agents |

**Output columns:** name, status, tool, tag, age, directory, transcript

## Reading Events

### `hcom events [flags]`

Query the event log with filters and SQL.

| Flag | Description |
|------|-------------|
| `--last <N>` | Last N events |
| `--all` | All events (no limit) |
| `--wait <SEC>` | Block until match or timeout |
| `--full` | Full event JSON output |
| `--sql <EXPR>` | Raw SQL WHERE clause |

**Filter flags (same flag = OR, different flags = AND):**

| Flag | Applies to | Description |
|------|-----------|-------------|
| `--agent <NAME>` | Any | Filter by instance name |
| `--type <TYPE>` | Any | message, status, life, bundle |
| `--status <STATUS>` | status events | active, listening, blocked, inactive |
| `--context <CTX>` | status events | tool:Bash, deliver:luna, tool:Write |
| `--file <PATTERN>` | status events | *.py, main.rs, *test* (glob in detail) |
| `--cmd <PATTERN>` | status events | =exact, ^prefix, $suffix, *glob, contains |
| `--from <NAME>` | message events | Sender name |
| `--mention <NAME>` | message events | @-mentioned agent |
| `--intent <TYPE>` | message events | request, inform, ack |
| `--thread <ID>` | message events | Thread identifier |
| `--reply-to <ID>` | message events | Referenced event ID |
| `--action <ACT>` | life events | created, started, ready, stopped, batch_launched |
| `--after <ISO8601>` | Any | Events after timestamp |
| `--before <ISO8601>` | Any | Events before timestamp |
| `--collision` | status | File edit collisions |

**Shortcuts:**
- `--idle <NAME>` expands to `--agent NAME --status listening`
- `--blocked <NAME>` expands to `--agent NAME --status blocked`

**Examples:**
```bash
hcom events --last 10
hcom events --agent luna --type message --from nova
hcom events --wait 120 --sql "msg_thread='review-1' AND msg_text LIKE '%DONE%'"
hcom events --file "*.py" --cmd "git*"
hcom events --idle luna
hcom events --wait 30 --idle "$codex_name"
```

## Event Subscriptions

### `hcom events sub [filters] [flags]`

Create reactive event subscriptions. Matches trigger a system message to the subscriber.

| Flag | Description |
|------|-------------|
| `--once` | Auto-delete after first match |
| `--for <AGENT>` | Deliver notification to specific agent |

**Examples:**
```bash
hcom events sub --idle worker-luna              # Notify when luna goes idle
hcom events sub --file "*.py" --once            # One-shot: next .py file edit
hcom events sub --collision                     # Two agents edit same file within 30s
hcom events sub --agent nova --status blocked   # Notify when nova gets blocked
```

## Reading Transcripts

### `hcom transcript [@name] [flags]`

Read an agent's conversation transcript.

| Flag | Description |
|------|-------------|
| `--full` | Complete conversation |
| `--detailed` | Include tool I/O, file edits, Bash output |
| `--last <N>` | Last N exchanges |
| `--range <N-M>` | Specific exchange range |

**Examples:**
```bash
hcom transcript @luna --full
hcom transcript @luna --full --detailed
hcom transcript @luna --last 3
hcom transcript @luna --range 5-10
```

## Listening for Messages

### `hcom listen [timeout] [flags]`

Block until a message arrives or timeout expires.

| Flag | Description |
|------|-------------|
| `<timeout>` | Seconds to wait (positional) |
| `--timeout <SEC>` | Seconds to wait (flag form) |

**Examples:**
```bash
hcom listen 30                                  # Wait 30s for any message
hcom listen --timeout 60 --name luna            # Wait as luna for 60s
```

## Agent Lifecycle

### `hcom kill <name|all> [flags]`

Kill agent and close terminal pane.

```bash
hcom kill luna --go          # Kill specific agent
hcom kill all --go           # Kill all agents
```

### `hcom stop <name> [flags]`

Graceful stop (preserves session for resume).

```bash
hcom stop luna --go
```

### `hcom r <name> [flags]`

Resume a stopped agent.

```bash
hcom r luna                  # Resume with original args
hcom r luna --tag reviewer   # Resume with new tag
```

### `hcom f <name> [flags]`

Fork an agent (clone session, new identity).

```bash
hcom f luna --tag reviewer   # Clone luna as a reviewer
```

## Context Bundles

### `hcom send --title <T> --description <D> [refs] -- <message>`

Send a structured context bundle with a message.

| Flag | Description |
|------|-------------|
| `--title <TEXT>` | Bundle title |
| `--description <TEXT>` | Bundle description |
| `--events <IDS>` | Event references (e.g., "42,43-50") |
| `--files <PATHS>` | File references (comma-separated) |
| `--transcript <REFS>` | Transcript refs with detail level (e.g., "1-20:full") |
| `--extends <BUNDLE_ID>` | Parent bundle for incremental context |

### `hcom bundle prepare`

Prepare a context bundle interactively.

**Transcript detail levels:**
- `normal`: truncated output
- `full`: complete text without tool I/O
- `detailed`: complete text with tool I/O and file edits

## Terminal Management

### `hcom term [name]`

View an agent's terminal screen.

### `hcom term inject <name> <text> [flags]`

Inject text into an agent's terminal.

| Flag | Description |
|------|-------------|
| `--enter` | Append carriage return (submit the text) |

```bash
hcom term inject luna "fix the bug" --enter
hcom term inject luna "/exit"                   # Without --enter, just types
```

## Workflow Scripts

### `hcom run <script> [args]`

Run a workflow script from `~/.hcom/scripts/` or bundled scripts.

```bash
hcom run debate --spawn --rounds 3 --tool claude
hcom run confess --fork --target luna
hcom run fatcow --path src/auth --focus "middleware,tokens"
hcom run my-custom-script "task description"
```

### `hcom run` (no args)

List available scripts with descriptions.

## Configuration

### `hcom config [key] [value]`

Get or set configuration values.

| Key | Description | Default |
|-----|-------------|---------|
| `timeout` | Default wait timeout (1-86400s) | 120 |
| `subagent_timeout` | Subagent keep-alive timeout | 300 |
| `terminal` | Terminal preset or custom command | auto-detect |
| `tag` | Default tag for all launched agents | none |
| `hints` | Hints passed to agents | none |
| `notes` | Notes appended to bootstrap | none |
| `claude_args` | Default args for Claude | none |
| `gemini_args` | Default args for Gemini | none |
| `codex_args` | Default args for Codex | none |
| `opencode_args` | Default args for OpenCode | none |
| `codex_sandbox_mode` | workspace/untrusted/danger-full-access/none | workspace |
| `auto_approve` | Auto-approve hcom commands | tool-dependent |
| `auto_subscribe` | Auto-subscribe presets | none |
| `name_export` | Custom env var name for instance | none |

```bash
hcom config terminal kitty-split
hcom config tag "team-alpha"
hcom config codex_sandbox_mode workspace
hcom config --edit          # Open TOML in editor
hcom config --reset         # Factory reset
hcom config --json          # JSON export
hcom config -i luna tag "reviewer"  # Per-instance override
```

## Relay (Cross-Device)

### `hcom relay new [flags]`

Create a new relay group.

```bash
hcom relay new                                    # Public broker
hcom relay new --broker mqtts://host:8883         # Private broker
hcom relay new --broker mqtts://host --password X # With auth
```

### `hcom relay connect <token> [flags]`

Join an existing relay group.

```bash
hcom relay connect AaGyw9Tl...
hcom relay connect <token> --password <secret>
```

### `hcom relay status/on/off`

```bash
hcom relay status             # Show connection state
hcom relay on                 # Enable relay
hcom relay off                # Disable relay
```

### `hcom relay daemon start/stop/restart/status`

Control the relay background worker.

## Diagnostics

### `hcom status`

Show hcom status: database, hooks, relay, active instances.

### `hcom hooks add <tool>`

Install hooks for a tool: `claude`, `codex`, `gemini`, `opencode`.

### `hcom hooks remove <tool>`

Remove hooks for a tool.
