# hcom

[![CI](https://github.com/aannoo/hcom/actions/workflows/ci.yml/badge.svg)](https://github.com/aannoo/hcom/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/aannoo/hcom)](https://github.com/aannoo/hcom/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

> Hook your coding agents together.

`hcom` is a Rust CLI that connects AI coding agents running in separate terminals. Agents can message each other, observe activity, share context bundles, spawn workers, resume or fork sessions, and coordinate across devices without changing how you normally use Claude Code, Codex, Gemini CLI, or OpenCode.

The local coordination layer is a SQLite database under `~/.hcom` or `HCOM_DIR`. Hooks and PTY wrappers write activity into that database, and hcom delivers messages back into participating agents when they are ready.

https://github.com/user-attachments/assets/1ce23ed9-f529-4be0-8124-816aa4c2fd43

## Fork Note

This repository is `RichelynScott/hcom`, a fork of `aannoo/hcom`. The source and live installed binary are currently aligned with upstream `v0.7.17`; local changes in this fork are documentation, project setup, and skill guidance unless otherwise noted.

## Install

```bash
brew install aannoo/hcom/hcom
```

Other supported install paths:

```bash
# macOS, Linux, WSL, Android/Termux
curl -fsSL https://github.com/aannoo/hcom/releases/latest/download/hcom-installer.sh | sh

# Python tool install
uv tool install hcom

# Source build
git clone https://github.com/aannoo/hcom.git
cd hcom
cargo build --release
```

Verify the install:

```bash
hcom status
hcom list
```

## Quickstart

Terminal 1:

```bash
hcom claude
```

Terminal 2:

```bash
hcom codex
```

Send a message from either terminal or from a shell:

```bash
hcom send @name --intent request -- What are you working on?
hcom events --last 5
```

Open the dashboard:

```bash
hcom
```

Inside an AI tool that was not launched by hcom, join manually:

```bash
hcom start
```

If you are inside a sandbox that cannot write to `~/.hcom`, isolate state in the current project:

```bash
export HCOM_DIR="$PWD/.hcom"
hcom start
```

## What hcom Provides

| Capability | What it does |
|---|---|
| Messaging | Send direct, multi-target, tag-prefix, broadcast, threaded, replied, and intent-tagged messages. |
| Observation | Read transcripts, terminal screens, command/file-edit events, status, and lifecycle history. |
| Event subscriptions | Get notified when an agent goes idle, blocks, edits a file, runs a command, or matches a SQL/event filter. |
| Context bundles | Package event IDs, files, and transcript ranges for handoffs without pasting large context into every message. |
| Lifecycle control | Launch, resume, fork, stop, or kill agents locally or over relay. |
| Workflow scripts | Run bundled or custom orchestration scripts through `hcom run`. |
| Cross-device relay | Sync trusted devices through an MQTT relay with encrypted payloads. |

## Mental Model

`hcom` tracks a few core objects:

- **Instance**: a named hcom participant, such as `luna` or `review-luna`.
- **Session**: a tool session bound to an instance, such as a Claude or Codex session ID.
- **Event**: immutable database record for a message, status update, lifecycle change, file edit, or relay result.
- **Message**: an event with a sender, text, scope, mentions, intent, optional reply ID, optional thread, and optional bundle.
- **Bundle**: a structured context package that references events, files, and transcript ranges.
- **Relay device**: a trusted remote device in the same relay group.

The normal data flow is:

```text
agent/tool -> hcom hook or PTY wrapper -> SQLite event log -> delivery hook/PTY -> target agent
```

## Supported Tools

| Tool | Automatic delivery | Launch form |
|---|---:|---|
| Claude Code | Yes, with hooks | `hcom claude` |
| Codex | Yes, with hooks | `hcom codex` |
| Gemini CLI | Yes, with hooks | `hcom gemini` |
| OpenCode | Yes, with plugin/hooks | `hcom opencode` |
| Any other process | Manual/ad-hoc | run `hcom start` inside the tool |

Install hooks:

```bash
hcom hooks add all
hcom hooks status
```

Restart the relevant tool after adding hooks. Without hooks, hcom still works in ad-hoc mode through `hcom start`, `hcom send`, and `hcom listen`.

## Command Map

Run `hcom <command> --help` for exact flags. Representative commands:

| Task | Commands |
|---|---|
| Dashboard | `hcom` |
| Launch agents | `hcom claude`, `hcom 3 codex`, `hcom 1 claude --tag review --headless` |
| Resume/fork | `hcom r <name>`, `hcom f <name>` |
| Kill/stop | `hcom kill <name>`, `hcom kill tag:<tag>`, `hcom stop <name>` |
| Message | `hcom send @name -- text`, `hcom send @a @b --intent request --thread t1 -- text` |
| Wait/listen | `hcom listen`, `hcom listen 30 --intent request`, `hcom listen --idle name` |
| Events | `hcom events --last 20`, `hcom events --agent name`, `hcom events sub --idle name` |
| Transcript | `hcom transcript name --last 20`, `hcom transcript search "pattern" --all` |
| Terminal | `hcom term name --json`, `hcom term inject name "text" --enter` |
| Bundles | `hcom bundle prepare`, `hcom bundle create "title" --description ...` |
| Config | `hcom config`, `hcom config terminal --info`, `hcom config -i name hints "..."` |
| Relay | `hcom relay new`, `hcom relay connect <token>`, `hcom relay status` |
| Scripts | `hcom run`, `hcom run docs`, `hcom run <script> --source` |
| Maintenance | `hcom status --logs`, `hcom update`, `hcom reset` |

### Messaging

```bash
hcom send @luna -- Hello
hcom send @luna @nova --intent request -- Can you review this?
hcom send @review- --thread audit-1 -- Start your pass
hcom send @luna --reply-to 42 --intent ack -- Fixed
hcom send @luna:BOXE -- Remote message
```

Target matching:

- `@luna` matches local base names and tag-prefixed names such as `review-luna`.
- `@review-` matches all agents with that tag prefix.
- `@luna:BOXE` targets a remote relay device.
- An underscore blocks prefix matching, so `@luna` does not match `luna_reviewer_1`.

Message intents:

- `request`: target should respond.
- `inform`: target responds only if useful.
- `ack`: acknowledgement of a request; requires `--reply-to`.

Threads let later messages reuse seeded recipients:

```bash
hcom send @a @b --thread triage-1 -- Start
hcom send --thread triage-1 -- Continue with the next step
```

### Events and Subscriptions

```bash
hcom events --last 20
hcom events --agent luna --type status
hcom events --cmd '^git' --agent luna
hcom events sub --idle luna
hcom events sub --file '*.py' --once
hcom events sub "type='message' AND msg_thread='triage-1'"
```

Subscriptions are stored in the local database and delivered as hcom messages when future events match. Built-in event fields include message metadata, status context/detail, lifecycle action, and file/command filters.

### Bundles

Bundles are structured handoff packets. They reference content instead of copying it into every message.

```bash
hcom bundle prepare
hcom bundle create "Auth bug handoff" \
  --description "State, evidence, and next step" \
  --events 120-135 \
  --files src/auth.rs,tests/auth.rs \
  --transcript 7-12:full

hcom send @reviewer \
  --title "Review this fix" \
  --description "Patch and context" \
  --events 120-135 \
  --files src/auth.rs \
  --transcript 7-12:normal \
  -- Please review this bundle.
```

Details:

- `bundle show <id>` displays metadata.
- `bundle cat <id>` expands bundle contents.
- `bundle chain <id>` shows lineage through `--extends`.
- Transcript detail levels are `normal`, `full`, and `detailed`.

### Workflow Scripts

`hcom run` executes bundled scripts compiled into hcom and user scripts from `~/.hcom/scripts/`. User scripts shadow bundled scripts with the same name.

```bash
hcom run
hcom run debate "Should we keep this API?"
hcom run <script> --source
hcom run docs --scripts
```

Script rules worth following:

- Parse and forward `--name`; hcom injects it for identity.
- Capture launched names from `Names: ...`.
- Use unique `--thread` values for concurrent workflows.
- Wait for agents to be `active` or `listening` before sending work.
- Use `trap` cleanup for launched workers.
- Use `--go` in unattended launch/kill commands to avoid confirmation prompts.
- Use `hcom listen` or `hcom events --wait`; do not rely on arbitrary sleeps.

## Runtime Files and Configuration

`HCOM_DIR` controls hcom state. Default:

```text
~/.hcom
```

Important paths:

| Path | Purpose |
|---|---|
| `$HCOM_DIR/hcom.db` | SQLite database |
| `$HCOM_DIR/config.toml` | User config |
| `$HCOM_DIR/config.env` | Legacy config file |
| `$HCOM_DIR/env` | Extra environment passed to launched agents |
| `$HCOM_DIR/.tmp/logs/hcom.log` | Runtime log |
| `$HCOM_DIR/.tmp/launch/` | Launch wrapper scripts |
| `$HCOM_DIR/.tmp/launched_pids.json` | PID tracking |
| `$HCOM_DIR/archive/` | Archived sessions |
| `$HCOM_DIR/scripts/` | User workflow scripts |

Config precedence for user-facing settings is:

```text
defaults < config.toml < HCOM_* environment variables < CLI flags
```

Relay secrets are intentionally file-only. The relay PSK is not exported into child process environments.

See [Configuration](docs/configuration.md) for config keys, `HCOM_DIR` isolation, terminal presets, and reset behavior.

## Hooks and Delivery

Hooks are installed into each tool's config area and are gated so they stay quiet when hcom is not in use.

Common hook locations:

- Claude Code: `~/.claude/settings.json`, or `$HCOM_DIR` parent `.claude/settings.json` in local mode.
- Codex: `~/.codex/config.toml` plus `~/.codex/hooks.json`, or `$HCOM_DIR` parent `.codex/`.
- Gemini: `~/.gemini/settings.json`, or `$HCOM_DIR` parent `.gemini/settings.json`.
- OpenCode: plugin path managed by hcom.

Delivery behavior:

- hcom-launched agents bind sessions through `HCOM_PROCESS_ID`.
- Vanilla sessions can join with `hcom start`.
- Hooks record status and tool activity.
- Pending direct messages are delivered at safe points, such as post-tool or prompt hooks.
- PTY delivery checks readiness and prompt-empty state before injecting text.
- Broadcasts do not wake dormant subagents; direct mentions do.

See [Hook System](docs/hook-system.md) and [Architecture](docs/architecture.md).

## Cross-Device Relay

Relay connects trusted devices over MQTT.

```bash
hcom relay new
hcom relay connect <token>
hcom relay status
hcom relay off
```

Custom broker:

```bash
hcom relay new --broker mqtts://host:port --password <broker-auth-secret>
hcom relay connect <token> --password <secret>
```

Security model:

- Payloads are end-to-end encrypted with XChaCha20-Poly1305.
- Join tokens contain the relay ID, broker URL, and raw PSK.
- The PSK is full authority for decrypting and publishing in that relay group.
- Relay is one full-trust domain. There are no per-device roles or read-only peers.
- Brokers and observers cannot read payloads, but can see metadata such as topic names, timing, and sizes.
- A leaked token/PSK cannot be revoked; create a new relay group and move trusted devices.

See [Relay](docs/relay.md) for operations, token format, limits, and incident response.

## Troubleshooting

```bash
hcom status
hcom status --logs
hcom hooks status
hcom list
hcom events --last 20
hcom term <name> --json
```

Common fixes:

- Hooks missing: run `hcom hooks add <tool>` and restart the tool.
- Sandbox cannot write `~/.hcom`: set `HCOM_DIR="$PWD/.hcom"`.
- Message target ambiguous: use the full tag-prefixed name or remote suffix.
- Workflow messages leaking: use a unique `--thread`.
- Stale state: inspect `hcom archive` before using reset.

Reset commands:

```bash
hcom reset        # archive current DB, clear active DB
hcom reset hooks  # remove hooks
hcom reset all    # stop all, clear DB, remove hooks, reset config
```

Treat reset as destructive. Inside AI tools, hcom shows a preview unless `--go` is provided.

## Development

This is a Rust binary crate with Python packaging through `maturin`.

Verification commands:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Key directories:

- `src/commands/`: CLI commands.
- `src/hooks/`: Claude, Codex, Gemini, and OpenCode integration.
- `src/db/`: SQLite schema and data access.
- `src/pty/`: terminal wrapping, screen parsing, and injection.
- `src/relay/`: MQTT sync, encryption, replay guard, RPC.
- `src/tui/`: dashboard.
- `src/transcript/`: transcript readers.
- `skills/hcom-agent-messaging/`: bundled agent skill and workflow examples.
- `tests/`: smoke, parser drift, PTY delivery, and relay tests.

See [Development](docs/development.md).

## Documentation

- [Architecture](docs/architecture.md)
- [Command Reference](docs/command-reference.md)
- [Configuration](docs/configuration.md)
- [Hook System](docs/hook-system.md)
- [Multi-Agent Workflows](docs/multi-agent-workflows.md)
- [Relay](docs/relay.md)
- [Development](docs/development.md)
- [DeepWiki MCP Usage](docs/deepwiki-mcp-usage.md)

## License

MIT. See [LICENSE](LICENSE).
