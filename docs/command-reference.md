# Command Reference

This reference is grouped by workflow. Run `hcom <command> --help` for exact current flags.

## Launch and Lifecycle

```bash
hcom [N] claude|gemini|codex|opencode [flags] [tool-args]
hcom r <target> [tool-args]
hcom f <target> [tool-args]
hcom kill <name>
hcom kill tag:<tag>
hcom kill all
hcom stop <name>
hcom start
hcom start --as <name>
hcom start --orphan <name|pid>
```

Launch flags:

| Flag | Purpose |
|---|---|
| `--tag <name>` | group launched agents under `tag-` display names |
| `--terminal <preset>` | choose terminal/pane preset |
| `--dir <path>` | launch in a working directory |
| `--headless` | run in background |
| `--pty` | force PTY wrapper |
| `--run-here` | launch in current terminal |
| `--hcom-prompt <text>` | initial prompt |
| `--hcom-system-prompt <text>` | system prompt |
| `--device <name>` | launch on a remote relay device |

Resume/fork targets can be names, stopped names, session UUIDs, or thread names. Remote fork requires `--dir` so the target device knows where to run.

Codex tool args are passed through hcom's Codex parser before launch. hcom recognizes Codex's hidden `--yolo` flag as a boolean sandbox override, equivalent in precedence to `--dangerously-bypass-approvals-and-sandbox`, so `hcom codex --yolo` does not also inherit the configured workspace sandbox mode.

Claude does not have a native `--yolo` flag, but hcom accepts it as an ergonomic alias for `--dangerously-skip-permissions`. `hcom claude --yolo` (and `hcom r <claude-session> --yolo`) parse successfully, get rewritten to `--dangerously-skip-permissions` before reaching the `claude` binary, and emit a one-line note to stderr at launch: `hcom: Claude session — accepting --yolo as --dangerously-skip-permissions.`

## Messaging

```bash
hcom send @name -- message text
hcom send @name1 @name2 -- message text
hcom send -- message text
hcom send @name --file brief.md
hcom send @name --base64 <encoded>
```

Everything after `--` is message text. Flags must come before `--`.

Target rules:

| Target | Meaning |
|---|---|
| `@luna` | local base name, also matches tag-prefixed `tag-luna` |
| `@tag-` | all agents with a tag prefix |
| `@tag-luna` | exact full display name |
| `@luna:BOXE` | remote relay instance |

Envelope flags:

| Flag | Meaning |
|---|---|
| `--intent request` | recipient should respond |
| `--intent inform` | recipient responds only if useful |
| `--intent ack` | acknowledgement; requires `--reply-to` |
| `--reply-to <id>` | link to event ID, local or remote |
| `--thread <name>` | group messages and reuse seeded recipients |
| `--from <name>` | external sender identity |
| `--name <name>` | hcom identity of command runner |

Inline bundle flags:

| Flag | Meaning |
|---|---|
| `--title <text>` | create and attach bundle |
| `--description <text>` | required with title |
| `--events <ids>` | event IDs or ranges |
| `--files <paths>` | comma-separated files |
| `--transcript <ranges>` | transcript ranges with detail levels |
| `--extends <id>` | parent bundle |

## Events and Listen

```bash
hcom events
hcom events --last 50
hcom events --agent luna --type status
hcom events --cmd '^git'
hcom events --file '*.rs'
hcom events --sql "type='message'"
hcom events --wait 120 --thread triage-1
hcom listen
hcom listen 30
hcom listen --idle luna
```

Filters with the same flag are ORed. Different flags are ANDed. Raw SQL is evaluated against `events_v`.

Subscription forms:

```bash
hcom events sub list
hcom events sub --idle luna
hcom events sub --file '*.py' --once
hcom events sub "type='message' AND msg_intent='request'"
hcom events unsub <id>
```

Useful fields:

- Base: `id`, `timestamp`, `type`, `instance`.
- Message: `msg_from`, `msg_text`, `msg_scope`, `msg_intent`, `msg_thread`, `msg_reply_to`.
- Status: `status_val`, `status_context`, `status_detail`.
- Lifecycle: `life_action`, `life_by`, `life_reason`.

## Transcript

```bash
hcom transcript <name>
hcom transcript <name> N
hcom transcript <name> N-M
hcom transcript <name> --last 20
hcom transcript <name> --full
hcom transcript <name> --detailed
hcom transcript timeline
hcom transcript search "pattern" --all
```

Use transcript ranges in messages and bundles instead of copying large conversation text.

## Bundles

```bash
hcom bundle
hcom bundle list --json
hcom bundle prepare
hcom bundle create "Title" --description "..." --events 1,2,5-10 --files a.rs,b.rs --transcript 3-14:normal
hcom bundle show <id>
hcom bundle cat <id>
hcom bundle chain <id>
```

Transcript detail levels:

- `normal`: truncated conversation.
- `full`: complete assistant responses.
- `detailed`: tools, edits, and richer execution detail.

## Terminal

```bash
hcom term
hcom term <name>
hcom term <name> --json
hcom term inject <name> "text"
hcom term inject <name> --enter
hcom term debug on
hcom term debug off
hcom term debug logs
```

JSON fields include `lines`, `size`, `cursor`, `ready`, `prompt_empty`, and `input_text`.

## Config

```bash
hcom config
hcom config <key>
hcom config <key> <value>
hcom config <key> --info
hcom config --edit
hcom config --reset
hcom config -i <name|self> [key] [value]
```

Per-agent keys include `tag`, `timeout`, `hints`, and `subagent_timeout`.

## Hooks

```bash
hcom hooks
hcom hooks status
hcom hooks add [claude|gemini|codex|opencode|all]
hcom hooks remove [claude|gemini|codex|opencode|all]
```

Restart the affected AI tool after adding hooks.

## Relay

```bash
hcom relay
hcom relay new
hcom relay connect <token>
hcom relay on
hcom relay off
hcom relay status
hcom relay daemon
hcom relay daemon start
hcom relay daemon stop
hcom relay daemon restart
```

Custom broker:

```bash
hcom relay new --broker mqtts://host:port --password <broker-auth-secret>
hcom relay connect <token> --password <secret>
```

## Scripts

```bash
hcom run
hcom run <script> [args]
hcom run <script> --source
hcom run docs
hcom run docs --cli
hcom run docs --config
hcom run docs --scripts
```

User scripts live in `~/.hcom/scripts/` and can be `*.sh` or `*.py`.

## Maintenance

```bash
hcom status
hcom status --logs
hcom update
hcom archive
hcom reset
hcom reset hooks
hcom reset all
```

`hcom reset` archives and clears the active database. `hcom reset all` also stops instances, removes hooks, resets config, and clears device identity. Treat reset commands as destructive.
