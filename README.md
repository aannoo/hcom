# hcom

[![CI](https://github.com/Solar2004/hcom/actions/workflows/ci.yml/badge.svg)](https://github.com/Solar2004/hcom/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/Solar2004/hcom)](https://github.com/Solar2004/hcom/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://github.com/Solar2004/hcom/blob/main/LICENSE)

> **Hook your coding agents together**

`hcom` is a CLI that agents can use to message, watch, and spawn each other across terminals. It integrates with Claude Code, Gemini, Codex, OpenCode, and Kilo Code without changing how you use them.

Use it to coordinate pipelines, run different AI CLIs as each other's subagents, or just instead of copy-paste.

Single Rust binary, no background services. Start an agent with `hcom` in front, then prompt normally.

https://github.com/user-attachments/assets/1ce23ed9-f529-4be0-8124-816aa4c2fd43

---

## Install

```bash
brew install Solar2004/hcom/hcom
```

<details><summary>Other install options</summary>

```bash
# Shell installer for macOS, Linux, Android (Termux), and WSL
curl -fsSL https://github.com/Solar2004/hcom/releases/latest/download/hcom-installer.sh | sh
```

```bash
# With PyPI
uv tool install hcom  # or: pip install hcom
```

</details>

---

## Quickstart

Terminal 1:

```bash
hcom claude   # codex / gemini / opencode / kilo
```

Terminal 2:

```bash
hcom codex
```

Prompt:

- `ask the other agent their favorite cake`
- `review what claude did and send it fixes`
- `spawn 3x gemini, split work, collect results`
- `fork yourself to investigate the bug and report back`

Open the TUI:

```bash
hcom
```

---

## What agents can do

**Message** each other in real-time: intent, replies, bundled context for handoffs.

**Observe** each other: transcripts, file edits, terminal screens, command history.

**Subscribe** to each other: notify on status changes, file edits, specific events. React automatically.

**Spawn**, **fork**, **resume**, **kill** each other, in any terminal emulator or headless.

---

## How it works

Hooks record activity to a local SQLite database and deliver messages from it.

```bash
agent → hooks → db → hooks → other agent
```

Messages arrive mid-turn (injected between tool calls) or wake idle agents immediately.

Each agent gets a queryable identity:

- name
- status (active, blocked, listening)
- inbox
- live terminal screen
- transcript in structured chunks
- event log of every status change, file edit, tool call

Agents can subscribe to events and react instantly. Collision detection is on by default: if two agents edit the same file within 30 seconds, both get notified.

Hooks go into config dirs under `~/` (or `HCOM_DIR`) on first run. If you aren't using hcom, the hooks do nothing.

Without hooks, any other AI tool can join by running `hcom start`. Any process can wake agents with `hcom send`.

### Antigravity (VS Code fork) extension

[Antigravity](https://github.com/antigravity-ai) is Google's VS Code fork with an
AI agent (Jetski). hcom includes a VS Code extension that bridges hcom agents
with Antigravity's chat panel:

```bash
# Install the extension into Antigravity
hcom hooks add antigravity

# Restart Antigravity to activate
```

Once installed:
- hcom messages arrive in Antigravity's chat panel automatically
- Type `@hcom list` in Antigravity chat to see active agents
- Type `@hcom @name message` to send messages to hcom agents
- Status bar shows connection status, click for details

The extension (~13KB) is compiled and embedded in the hcom binary — no npm
or TypeScript needed on the user's machine.

---

## Terminal

Every agent runs in a real terminal you can see, scroll, and interrupt. Any emulator works for spawning; **kitty**, **wezterm**, **tmux**, and **cmux** also support closing panes from `hcom kill`.

To configure a custom terminal open/close setup, tell an agent to run:

```bash
hcom config terminal --info
```

---

## Cross-device

Connect agents across machines via MQTT relay.

```bash
hcom relay new               # get token
hcom relay connect <token>   # on each device
```

```bash
hcom relay status            # check connection
hcom relay off|on            # toggle
```

<details>
<summary>Relay Security</summary>

### Security

- Relay payloads are end-to-end encrypted. Brokers do not see data.
- Treat the join token like an SSH key or API key.
- If the token may have leaked, run `hcom relay off --all` to disconnect all devices.
- Use a private/custom/self-hosted broker with `--broker` and `--password` for better security.

### Security model

`hcom relay` is one trust domain for one operator's devices. Membership is all-or-nothing. There are no scoped roles, read-only peers, or per-device permissions.

Relay payloads use a shared PSK with XChaCha20-Poly1305. The encryption binds each payload to the relay, topic, and timestamp. A replay guard drops duplicate envelopes inside a freshness window.

Brokers and network observers cannot read or forge payloads without the PSK. They can still see metadata: topic names, timing, message sizes, and connection patterns.

### What the token means

The join token contains the relay ID, broker URL, and raw PSK. hcom does not ask a server to validate it. It has no expiry, no scope, and no revocation list.

On public brokers, a leaked token gives an attacker full control of the relay. They can decrypt captured traffic, publish authenticated relay traffic, send text to listening agents, launch agents on enrolled devices, kill running agents, and use remote relay RPCs. If those agents can run tools, treat that as shell access on every enrolled device in the relay.

On private brokers with `--password`, the token still leaks the PSK, so captured traffic is still exposed. But the token alone is not enough to publish unless the attacker also has the broker password. Use a private broker when broker-side access control matters, or when the metadata shape of your traffic is itself sensitive. `--password` is broker access control, not another layer of message encryption.

### Limits by design

- Forward secrecy. A leaked PSK can decrypt old captured traffic.
- Per-device attribution inside a relay. Sender identity is routing metadata, not authorization. Every enrolled device speaks with full authority.
- Prompt injection from an authenticated peer. Enrollment is total trust — a peer can launch, kill, and drive agents via RPC, not just send messages. Only enroll devices you would give shell access to.
- Local OS compromise. hcom trusts the local user account and `~/.hcom/config.toml`. It does not defend against another user on the same account or malware with filesystem access.

### Storage

The PSK is stored in `~/.hcom/config.toml`. On Unix, hcom writes that file with mode `0600`.

hcom keeps the PSK out of environment variables. Remote `config_get` and `config_set` refuse `relay_psk`, `relay_token`, `relay_id`, and the broker URL. `hcom relay status` shows only a short fingerprint so two devices can verify they share the same key without printing it.

Anyone who can read that file — another user on the same OS account, malware, or a backup written without preserving permissions — has the full PSK.

### Incident response

Run `hcom relay off --all`. It asks every reachable trusted peer to disable the relay, then disables it locally, so your agents stop acting on attacker messages. It is best-effort damage control, not containment: the attacker's device ignores the request.

The PSK cannot be revoked. There is no server to notify and no denylist to update. Anyone who has the PSK can keep using the old relay until you stop using it.

To keep using relay after a leak, create a new relay with `hcom relay new` and move every trusted device to the new token. Rotation also changes the `relay_id`, so retained state on the old broker topics is orphaned.

</details>

---

## Troubleshoot

```bash
hcom status                  # diagnostics
hcom reset all               # clear and archive: database + hooks + config
```

---

## Uninstall

```bash
hcom hooks remove            # safely remove all hcom hooks
brew uninstall hcom          # or: rm $(which hcom)
```

---

## Reference

<details>
<summary>Tools</summary>

### Supported tools

| Tool | Message delivery | Connect |
|---|---|---|
| Claude Code | automatic | `hcom claude` |
| Gemini CLI | automatic | `hcom gemini` |
| Codex CLI | automatic | `hcom codex` |
| OpenCode | automatic | `hcom opencode` |
| Kilo Code | automatic | `hcom kilo` |
| Cline | automatic | `hcom cline` |
| Anything else | manual via `hcom listen` | `hcom start` (run inside tool) |

```bash
hcom r <session_id>           # Resume a session started outside hcom
hcom f <session_id>           # Fork a session in hcom
```

#### Claude Code headless and subagents

Detached background processes in print mode stay alive. Manage through the TUI.

```bash
hcom claude -p 'say hi in hcom'
```

For subagents, run `hcom claude`, then prompt:

> run 2x task tool and get them to talk to each other in hcom

</details>


<details>
<summary>CLI</summary>

### CLI commands

What you might type from a shell. Agents run their own commands that they learn from the hcom CLI primer (~700 tokens) at launch. `hcom <command> --help` for full flags.

### Spawn

```bash
hcom [N] claude|gemini|codex|opencode|kilo|cline   # launch N agents
hcom r <name|session_id>                # resume agent
hcom f <name|session_id>                # fork session
hcom kill <name|tag:T|all>              # kill + close terminal pane
```

hcom launch flags:

| Flag | Purpose |
|---|---|
| `--tag <name>` | Group label — agents can be addressed as `@tag` |
| `--terminal <preset>` | Where windows open: `default` (auto-detect), `kitty`, `wezterm`, `tmux`, `cmux`, `iterm`, etc… |
| `--dir <path>` | Directory where the agent launches |
| `--headless` | Run in background with no terminal window |
| `--device <name>` | Spawn on a remote device (via relay) |
| `--hcom-prompt <text>` | Initial user prompt |
| `--hcom-system-prompt <text>` | Append to system prompt |

Anything else is forwarded to the tool: `--model sonnet`, `--yolo`, etc.

### Other commands

```bash
hcom                                # TUI dashboard
hcom send -b @luna -- hey           # one-off message to an agent
hcom list                           # show all active agents
hcom term [name]                    # view/inject into an agent's PTY screen
hcom events --wait <filters>         # Block until match for scripting
hcom update                         # update hcom version
```

`hcom --help` for all commands.

</details>

<details>
<summary>Config</summary>

### Configuration

Config lives in `~/.hcom/config.toml`. Precedence: defaults < `config.toml` < env vars.

```bash
hcom config                           # show all values with sources
hcom config <key>                     # get
hcom config <key> <value>             # set
hcom config <key> --info              # detailed help for a key
hcom config -i <name> <key> <value>   # per-agent override at runtime
```

### Keys

| Key | Purpose |
|---|---|
| `tag` | Group label — launched agents become `tag-name` |
| `hints` | Text appended to every message the agent receives |
| `notes` | Text appended to bootstrap (one-time, at launch) |
| `auto_approve` | Auto-approve safe hcom commands (send/list/events/…) |
| `auto_subscribe` | Event subscription presets: `collision`, `created`, `stopped`, `blocked` |
| `name_export` | Export instance name to a custom env var |
| `terminal` | Where new agent windows open (`hcom config terminal --info`) |
| `timeout` | Idle timeout for headless/vanilla Claude (seconds) |
| `subagent_timeout` | Keep-alive for Claude subagents (seconds) |
| `claude_args` / `gemini_args` / `codex_args` / `opencode_args` / `kilo_args` | Default args passed to the tool |

### Scope

```bash
hcom config tag mycrew                          # global
hcom config -i luna hints "respond in JSON"     # per-agent
HCOM_TAG=dev hcom 3 claude                      # per-launch env
```

### Per-project isolation

```bash
export HCOM_DIR="$PWD/.hcom"    # isolate state + hooks to this folder
hcom hooks remove && rm -rf "$HCOM_DIR"
```

Run `hcom config <key> --info` or `hcom run docs --config` for the full per-key reference.

Edit `~/.hcom/env` to set external env vars passed to every launched agent.

</details>

<details>
<summary>Workflow Scripts</summary>

### Multi-agent workflows

Bundled and user scripts (`~/.hcom/scripts/`) for multi-agent patterns:

```bash
hcom run                   # list available scripts
hcom run debate "topic"    # run one
hcom run docs              # tell agent to run this to create any new workflow
```

### Included Scripts

Tell agent to run them:

**`hcom run confess`** — An agent (or background clone) writes an honesty self-eval. A spawned calibrator reads the target's transcript independently. A judge compares both reports and sends back a verdict via hcom message.

**`hcom run debate`** — A judge spawns and sets up a debate with existing agents. It coordinates rounds in a shared thread where all agents see each other's arguments, with shared context of workspace files and transcripts.

**`hcom run fatcow`** — headless agent reads every file in a path, subscribes to file edit events to stay current, and answers other agents on demand.

Custom scripts: drop `*.sh` or `*.py` into `~/.hcom/scripts/` — auto-discovered, override bundled scripts of the same name. Ask an agent to author one; `hcom run docs --scripts` is the authoring guide.

</details>

<details>
<summary>Build</summary>

### Building from Source

```bash
# Prerequisites: Rust 1.86+

git clone https://github.com/Solar2004/hcom.git
cd hcom
cargo build
cargo test
```

### Using local build

Two options:

**Symlink** — simple, dev build is global.

```bash
ln -sf $(pwd)/target/debug/hcom ~/.cargo/bin/hcom
```

**dev_root** — works regardless of how hcom was installed (brew, pip, etc.); picks the newer of debug/release automatically:

```bash
hcom config dev_root $(pwd)
hcom config dev_root --unset  # revert
hcom status    # run local build
```

For concurrent worktrees, scope each to its own DB:

```bash
HCOM_DIR=$PWD/.hcom HCOM_DEV_ROOT=$PWD hcom claude
```

### Testing a fork alongside the original

To test a custom fork (e.g., with kilo support) without replacing your installed `hcom`:

```bash
# 1. Clone your fork
git clone https://github.com/Solar2004/hcom.git ~/hcom-kilo
cd ~/hcom-kilo
cargo build

# 2. Use dev_root to point at the fork build
hcom config dev_root $(pwd)
hcom status          # now runs your fork

# 3. Revert to original anytime
hcom config dev_root --unset
```

Or use a symlink swap:

```bash
# Point to your fork build
ln -sf ~/hcom-kilo/target/debug/hcom ~/.cargo/bin/hcom-kilo

# Run either
hcom          # original installed version
hcom-kilo     # your fork
```

</details>


---

## Contributing

Issues and PRs welcome. The codebase is Rust.

```bash
cargo build && cargo test
hcom config dev_root $(pwd)
hcom status
```

---

## License

[MIT](LICENSE)
