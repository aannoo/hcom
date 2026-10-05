<div align="center">

# `hcom`

*Hook your coding agents together*

[![CI](https://github.com/aannoo/hcom/actions/workflows/ci.yml/badge.svg)](https://github.com/aannoo/hcom/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/aannoo/hcom)](https://github.com/aannoo/hcom/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://github.com/aannoo/hcom/blob/main/LICENSE)

</div>

**CLI tool that agents use to message, watch, and spawn each other across terminals.**

Start an agent with `hcom` in front, then prompt normally.

Use it to coordinate multi-agent pipelines, run different AI CLIs as each other's subagents, or just to avoid copy-pasting.

Works with: `claude`, `codex`, `opencode`, `copilot`, `qoder`, `grok`, `pi`, `omp`, `agy`, `cursor`, `kimi`, `kilo`, `gemini`

https://github.com/user-attachments/assets/1ce23ed9-f529-4be0-8124-816aa4c2fd43

## Install

**python -** macOS, Linux, Windows:

```bash
uv tool install hcom
```

**homebrew -** macOS, Linux:

```bash
brew install aannoo/hcom/hcom
```

<details>
<summary>Other install options</summary>

```bash
# macOS, Linux, Android
curl -fsSL https://github.com/aannoo/hcom/releases/latest/download/hcom-installer.sh | sh
```

```powershell
# Windows
irm https://github.com/aannoo/hcom/releases/latest/download/hcom-installer.ps1 | iex
```

```bash
# Update any existing install
hcom update
```

</details>

## Quickstart

<table>
<tr>
<td width="50%" valign="top">

**Terminal 1:**

```bash
hcom claude
```

</td>
<td width="50%" valign="top">

**Terminal 2:**

```bash
hcom codex
```

</td>
</tr>
</table>

<b>Prompt:</b>

<details>
<summary><code>ask the other agent their favorite cake</code></summary>

- `review what claude did and send it fixes`

- `spawn 3x opencode, split work, collect results`

- `fork yourself to investigate the bug and report back`

- `when codex goes idle, send it the next task`

</details>

**Open the TUI dashboard:**

```bash
hcom
```

## What agents can do

- **Message** each other in real time: mid-turn or wake immediately when idle

- **Observe** each other: status, transcripts, file edits, live terminal screens, command history.

- **Subscribe** and notify on status changes, file edits, collisions, specific events. React automatically.

- **Spawn**, **fork**, **resume**, **kill** in any terminal emulator or headless.

## How it works

Hooks record activity to a local SQLite database and deliver messages from it.

```text
agent → hooks → db → hooks → other agent
```

Hooks activate only when an agent is launched with `hcom` in front. Normal usage is unaffected.

<details>
<summary>Any other AI tool without hooks can join by running <code>hcom start</code></summary>

In any CLI tool, prompt:

```text
> run this command: `hcom start`
```

Keep it listening for messages:

```text
> stay connected to hcom
```

</details>

<details>
<summary>Any process can wake agents with <code>hcom send</code></summary>

Send messages from any process/script:

```bash
hcom send -b @luna -- "wake up and do this task"
```

Chain with `hcom events`:

```bash
hcom events --idle luna --wait 600 && hcom send -b @nova -- "luna is done, review it"
```

</details>

## Terminal

Every agent runs in a real terminal you can see, scroll, and interrupt. Any emulator works for spawning. **kitty**, **wezterm**, **tmux**, **zellij**, **waveterm**, **cmux**, **herdr** also support closing panes from `hcom kill`.

To configure a custom terminal open/close setup, tell an agent to run:

```bash
hcom config terminal --info
```

## Cross-device

Connect agents across machines via MQTT relay.

```bash
hcom relay new               # get token
hcom relay connect <token>   # on each device
```

```bash
hcom relay status   # check connection
hcom relay off|on   # toggle
```

> Treat the token like an API/SSH key. See [SECURITY.md](SECURITY.md)

## Troubleshoot

```bash
hcom status   # diagnostics
```

```bash
hcom reset all   # clear and archive: database + hooks + config
```

## Uninstall

Safely remove all hcom hooks:

```bash
hcom hooks remove
```

Then remove binary:

```bash
brew uninstall hcom
# or: uv tool uninstall hcom
# or: rm "$(which hcom)"
```

---

## Reference

<details>
<summary><strong>Tools</strong></summary>

### Supported tools

| Tool | Message delivery | Connect |
|---|---|---|
| Claude Code | automatic | `hcom claude` |
| Gemini CLI | automatic | `hcom gemini` |
| Codex CLI | automatic | `hcom codex` |
| Antigravity CLI | automatic | `hcom agy` |
| OpenCode | automatic | `hcom opencode` |
| Kilo Code | automatic | `hcom kilo` |
| Pi | automatic | `hcom pi` |
| Oh My Pi | automatic | `hcom omp` |
| Cursor CLI | automatic | `hcom cursor-agent` |
| Kimi | automatic | `hcom kimi` |
| Copilot CLI | automatic | `hcom copilot` |
| Qoder CLI | automatic | `hcom qoder` |
| Grok Build | automatic | `hcom grok` |
| Anything else | manual via `hcom listen` | `hcom start` (run inside tool) |

```bash
hcom r <session_id>   # Resume a session started outside hcom
hcom f <session_id>   # Fork a session in hcom
```

#### Codex native delivery (opt-in)

```bash
hcom config codex_native_delivery 1
```

By default hcom wakes an idle Codex by typing into its terminal. With this on, each Codex agent gets a private `codex app-server` and the terminal UI attaches to it with `--remote`; hcom wakes the agent through Codex's own queue on whichever thread the UI is showing. Drafts in the composer are never touched and the first message can arrive before the first prompt. Costs one extra Codex process per agent and relies on experimental Codex APIs. Unix only.

For npm-style installs (npm, bun, nvm, ...) the private server runs the native Codex binary directly, without a Node launcher in front of it. Other installs use the original command.

Launches the private server can't serve fall back to terminal delivery: `--profile`, `--remote`, `--no-daemon`, `--add-dir`, `--worktree`, and resume/fork with explicit permission flags (a remote Codex resumes with the thread's saved permissions).

#### Claude Code headless and subagents

Detached background processes in print mode stay alive. Manage through the TUI.

```bash
hcom claude -p 'say hi in hcom'   # print mode (separate Agent SDK credits)
hcom claude --headless            # Run normal claude in background pty (works for any tool)
```

For subagents, run `hcom claude`, then prompt:

> run 2x task tool and get them to talk to each other in hcom

</details>

<details>
<summary><strong>CLI</strong></summary>

### CLI commands

What you might type from a shell. Agents run their own commands that they learn from the hcom CLI primer (~700 tokens) at launch. `hcom <command> --help` for full flags.

#### Spawn

```bash
hcom [N] claude|gemini|codex|agy|opencode|kilo|pi|omp|cursor-agent|kimi|copilot|qoder|grok   # launch N agents
hcom r <name|session_id>     # resume agent
hcom f <name|session_id>     # fork session
hcom kill <name|tag:T|all>   # kill + close terminal pane
```

hcom launch flags:

| Flag | Purpose |
|---|---|
| `--tag <name>` | Group label — agents can be addressed as `@tag` |
| `--terminal <preset>` | Where windows open: `default` (auto-detect), `kitty`, `wezterm`, `tmux`, `cmux`, `iterm`, etc… |
| `--dir <path>` | Directory where the agent launches |
| `--headless` | Run in background pty with no terminal window |
| `--device <name>` | Spawn on a remote device (via relay) |
| `--hcom-prompt <text>` | Initial user prompt |
| `--hcom-system-prompt <text>` | Append to system prompt |

Anything else is forwarded to the tool: `--model sonnet`, `--yolo`, etc.

#### Other commands

```bash
hcom                           # TUI dashboard
hcom send -b @luna -- hey      # one-off message to an agent
hcom list                      # show all active agents
hcom term [name]               # view/inject into an agent's PTY screen
hcom events --wait <filters>   # Block until match for scripting
hcom update                    # update hcom version
```

`hcom run docs --cli` for all commands.

</details>

<details>
<summary><strong>Config</strong></summary>

### Configuration

Config lives in `~/.hcom/config.toml`. Precedence: defaults < `config.toml` < env vars.

```bash
hcom config                           # show all values with sources
hcom config <key>                     # get
hcom config <key> <value>             # set
hcom config <key> --info              # detailed help for a key
hcom config -i <name> <key> <value>   # per-agent override at runtime
```

#### Keys

| Key | Purpose |
|---|---|
| `tag` | Group label — launched agents become `tag-name` |
| `hints` | Text appended to every message the agent receives |
| `notes` | Text appended to bootstrap (one-time, at launch) |
| `auto_approve` | Auto-approve safe hcom commands (send/list/events/…) |
| `auto_subscribe` | Event subscription presets: `collision`, `created`, `stopped`, `blocked` |
| `name_export` | Export instance name to a custom env var |
| `title_mode` | Terminal/tab title behavior: `combined` (default), `label`, or `off` |
| `terminal` | Where new agent windows open (`hcom config terminal --info`) |
| `timeout` | Idle timeout for headless Claude (seconds) |
| `subagent_timeout` | Keep-alive for Claude subagents (seconds) |
| `claude_args` / `gemini_args` / `codex_args` / `opencode_args` / `kilo_args` / `pi_args` / `omp_args` / `cursor_args` / `kimi_args` / `copilot_args` / `qoder_args` / `grok_args` | Default args passed to the tool |

#### Scope

```bash
hcom config tag mycrew                        # global
hcom config -i luna hints "respond in JSON"   # per-agent
HCOM_TAG=dev hcom 3 claude                    # per-launch env
```

#### Per-project isolation

```bash
export HCOM_DIR="$PWD/.hcom"   # isolate hcom state (db, logs) to this folder
rm -rf "$HCOM_DIR"             # clean up
```

Run `hcom config <key> --info` or `hcom run docs --config` for the full per-key reference.

Edit `~/.hcom/env` to set external env vars passed to every launched agent.

</details>

<details>
<summary><strong>Workflow Scripts</strong></summary>

### Multi-agent workflows

Bundled and user scripts (`~/.hcom/scripts/`) for multi-agent patterns:

```bash
hcom run                  # list available scripts
hcom run debate "topic"   # run one
hcom run docs             # tell agent to run this to create any new workflow
```

#### Included scripts

Tell agent to run them:

- **`hcom run confess`** — An agent (or background clone) writes an honesty self-eval. A spawned calibrator reads the target's transcript independently. A judge compares both reports and sends back a verdict via hcom message.

- **`hcom run debate`** — A judge spawns and sets up a debate with existing agents. It coordinates rounds in a shared thread where all agents see each other's arguments, with shared context of workspace files and transcripts.

- **`hcom run fatcow`** — headless agent reads every file in a path, subscribes to file edit events to stay current, and answers other agents on demand.

- **`hcom run onidle`** — waits for an agent to go idle, then types text into another agent (`hcom run onidle luna nova 'luna is done, review it'`) or launches a new one with it as the prompt (`hcom run onidle luna codex 'review what luna just did'`).

Custom scripts: drop `*.sh` or `*.py` into `~/.hcom/scripts/` — auto-discovered, override bundled scripts of the same name. Ask an agent to author one; `hcom run docs --scripts` is the authoring guide.

</details>

## Contributing

Issues and PRs welcome. Build from source and dev setup: [CONTRIBUTING.md](CONTRIBUTING.md)

## License

[MIT](LICENSE)
