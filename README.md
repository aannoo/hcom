# hcom

[![PyPI](https://img.shields.io/pypi/v/hcom)](https://pypi.org/project/hcom/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/Built_with-Rust-dea584)](https://www.rust-lang.org/)
[![GitHub stars](https://img.shields.io/github/stars/aannoo/hcom)](https://github.com/aannoo/hcom/stargazers)

**Multi-agent communication for your terminal.** hcom connects AI coding agents running in separate terminals so they can message, watch, and spawn each other. No more isolated contexts, repeated decisions, or colliding file edits.

![demo](https://raw.githubusercontent.com/aannoo/hcom/refs/heads/assets/screencapture-new-new.gif)

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/aannoo/hcom/main/install.sh | sh
```

<details><summary>Or with pip / uv</summary>

```bash
pip install hcom        # or
uv tool install hcom
```

</details>

## Quickstart

Launch agents with `hcom` in front:

```bash
hcom claude
hcom gemini
hcom codex
hcom opencode
```

Tell any agent:

> send a message to claude

Open the TUI dashboard:

```bash
hcom
```

## How it works

Hooks record activity and deliver messages between agents through a local SQLite database:

```
agent --> hooks --> db --> hooks --> other agent
```

- Messages arrive mid-turn or wake idle agents immediately
- If 2 agents edit the same file within 30 seconds, both get collision notifications
- Agents are addressable by name, tool, terminal, branch, cwd, or custom tag

## Capabilities

| Capability | Command |
|---|---|
| Message each other (intents, threads, broadcast, @mentions) | `hcom send` |
| Read each other's transcripts (ranges and detail levels) | `hcom transcript` |
| View terminal screens, inject text/enter for approvals | `hcom term` |
| Query event history (file edits, commands, status, lifecycle) | `hcom events` |
| Subscribe and react to each other's activity | `hcom events sub` |
| Spawn, fork, resume agents in new terminal panes | `hcom N claude\|gemini\|codex\|opencode`, `hcom r`, `hcom f` |
| Kill agents and close their terminal panes/sessions | `hcom kill` |
| Build context bundles (files, transcript, events) for handoffs | `hcom bundle` |

### Example prompts

> when codex goes idle send it the next task

> watch gemini's file edits, review each and send feedback if any bugs

> fork yourself to investigate the bug and report back

> find which agent worked on terminal_id code, resume them and ask why it sucks

## Supported tools

| Tool | Message delivery |
|------|------------------|
| **Claude Code** (including subagents) | Automatic |
| **Gemini CLI** | Automatic |
| **Codex** | Automatic |
| **OpenCode** | Automatic |
| Any AI tool that can run shell commands | Manual -- tell agent `hcom start` |
| Any process | Fire and forget: `hcom send <message> --from botname` |

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

## Multi-agent workflows

Built-in workflow scripts you can run out of the box:

| Script | What it does |
|--------|-------------|
| `hcom run confess` | An agent writes an honesty self-eval. A spawned calibrator reads the target's transcript independently. A judge compares both reports and sends back a verdict. |
| `hcom run debate` | A judge spawns a debate with existing agents. It coordinates rounds in a shared thread where all agents see each other's arguments, with shared context of workspace files and transcripts. |
| `hcom run fatcow` | A headless agent reads every file in a path, subscribes to file edit events to stay current, and answers other agents on demand. |

Create your own by prompting:

> "read `hcom run docs` then make a script that does X"

User scripts go in `~/.hcom/scripts/` (`.sh` or `.py`).

## Terminal support

Spawning works with any terminal emulator. Closing/killing has full support for **kitty**, **wezterm**, and **tmux**.

```bash
hcom config terminal kitty    # set your terminal
```

Run `hcom config terminal --info` for the full list of presets and custom command setup.

## Cross-device sync

Connect agents across machines via MQTT relay:

```bash
# Create a relay group
hcom relay new

# On each device
hcom relay connect <token>
```

## Installation details

Hooks install into `~/` (or `HCOM_DIR`) on first launch or via `hcom start`.

```bash
hcom hooks remove                  # safely remove only hcom hooks
hcom status                        # diagnostics
```

```bash
HCOM_DIR=$PWD/.hcom                # sandbox / project-local mode
```

## Reference

<details>
<summary>CLI commands</summary>

```
hcom (hook-comms) v0.7.4 - multi-agent communication

Usage:
  hcom                                TUI dashboard
  hcom <command>                      Run command

Launch:
  hcom [N] claude|gemini|codex|opencode [flags] [tool-args]
  hcom r <name>                       Resume stopped agent
  hcom f <name>                       Fork agent session
  hcom kill <name(s)|tag:T|all>       Kill + close terminal pane

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
```

Run `hcom <command> --help` for detailed usage on any command.

</details>

<details>
<summary>Configuration</summary>

```
File: ~/.hcom/config.toml
Precedence: defaults < config.toml < env vars

Commands:
  hcom config                 Show all values
  hcom config <key> <val>     Set value
  hcom config <key> --info    Detailed help for a setting
  hcom config --edit          Open in $EDITOR

Per-agent: hcom config -i <name|self> [key] [val]

Keys:
  tag                  Group/label (agents become tag-*)
  terminal             Where new agent windows open
  hints                Text appended to all messages agent receives
  notes                Notes appended to agent bootstrap
  subagent_timeout     Subagent keep-alive seconds after task
  auto_approve         Auto-approve safe hcom commands
  auto_subscribe       Event auto-subscribe presets (default: collision)
  name_export          Export agent name to custom env var
  claude_args / gemini_args / codex_args / opencode_args
```

See `hcom config <key> --info` for detailed help on each setting.

</details>

<details>
<summary>Custom scripts</summary>

```
Location:    ~/.hcom/scripts/
File types:  *.sh (bash), *.py (python3)

User scripts shadow bundled scripts with the same name.
Drop a file and run: hcom run <name>

View source of any script:
  hcom run <name> --source

Full reference:
  hcom run docs
```

</details>

## Build from source

```bash
# Prerequisites: Rust 1.86+
git clone https://github.com/aannoo/hcom.git
cd hcom
./build.sh

# Put binary on PATH
ln -sf "$(pwd)/bin/hcom" ~/.local/bin/hcom
```

## Contributing

Issues and PRs welcome. The codebase is Rust -- `./build.sh` builds, runs tests, and copies the binary.

## License

[MIT](LICENSE)
