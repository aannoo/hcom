# Public DeepWiki Source Pack: aannoo/hcom

Created: 2026-05-22

Scope: source material for rewriting this fork's README, reference docs, and bundled `skills/hcom-agent-messaging` skill. This pack is based only on the public DeepWiki MCP server results for upstream `aannoo/hcom`. It does not validate differences in the local fork.

## Public MCP Calls Made

Only the public `deepwiki` MCP namespace was used for repository intelligence. No calls were made to `mcp__deepwiki_local__`, `deepwiki-local`, or `mcp__deepwiki_local__.*`.

1. `mcp__deepwiki__.read_wiki_structure`
   - Args: `{"repoName":"aannoo/hcom"}`
   - Purpose: retrieve the wiki table of contents.
2. `mcp__deepwiki__.read_wiki_contents`
   - Args: `{"repoName":"aannoo/hcom"}`
   - Purpose: retrieve the generated wiki contents. The public tool has no page/section argument, so this was the all-pages request supported by the public endpoint.
3. `mcp__deepwiki__.ask_question`
   - Args: `{"repoName":"aannoo/hcom","question":"For a source pack to rewrite the hcom README/docs/skill, summarize hcom command semantics. Cover command families, representative commands and flags, launch forms, messaging forms, query/events/transcript/status commands, terminal commands, config/relay/run/bundle/fork/resume/kill commands if present. Include file/module references and line references where DeepWiki has them. Call out uncertainty."}`
4. `mcp__deepwiki__.ask_question`
   - Args: `{"repoName":"aannoo/hcom","question":"For a source pack to rewrite the hcom README/docs/skill, summarize hcom configuration precedence and runtime files. Cover HCOM_DIR, ~/.hcom paths, config files and precedence, per-instance config, terminal presets, scripts/bundles/logs/db/hooks files, status diagnostics, reset/update/install behavior if relevant. Include file/module references and line references where DeepWiki has them. Call out uncertainty."}`
5. `mcp__deepwiki__.ask_question`
   - Args: `{"repoName":"aannoo/hcom","question":"For a source pack to rewrite the hcom README/docs/skill, summarize hook internals and session/identity binding. Cover supported tools, hook events, how hooks are installed/dispatch, session_bindings vs process_bindings, identity resolution, startup/bootstrap, status/heartbeat, subagent handling, and delivery gating. Include file/module references and line references where DeepWiki has them. Call out uncertainty."}`
6. `mcp__deepwiki__.ask_question`
   - Args: `{"repoName":"aannoo/hcom","question":"For a source pack to rewrite the hcom README/docs/skill, summarize message delivery, event subscriptions, bundles, context sharing, forking/resume/subagents. Cover delivery logic, scopes/mentions, intents/threads/replies, unread cursors, subscriptions/request watches/collisions, bundle create/show/cat/chain/prepare and inline send bundle refs, transcript use, fork/resume semantics. Include representative commands, file/module references and line references where DeepWiki has them. Call out uncertainty."}`
7. `mcp__deepwiki__.ask_question`
   - Args: `{"repoName":"aannoo/hcom","question":"For a source pack to rewrite the hcom README/docs/skill, summarize hcom relay security and cross-device synchronization. Cover relay architecture, MQTT topics, relay_id/token/psk, encryption algorithm, device IDs, event sync, remote addressing, remote launch/fork/control, CLI commands, config/storage, threat model and limitations. Include file/module references and line references where DeepWiki has them. Call out uncertainty."}`
8. `mcp__deepwiki__.ask_question`
   - Args: `{"repoName":"aannoo/hcom","question":"For a source pack to rewrite the hcom README/docs/skill, summarize build, test, packaging, installation, and distribution for hcom. Cover Rust crate, Python/maturin packaging, CI/release workflows, install script/Homebrew/PyPI/source install, supported platforms/targets, required verification commands, tests organization, important dependencies, version/update behavior. Include file/module references and line references where DeepWiki has them. Call out uncertainty."}`

## Public Wiki TOC

- 1 Overview
- 2 Getting Started
- 3 Core Concepts
- 4 Architecture
  - 4.1 CLI Entry Points and Command Routing
  - 4.2 Database and Event Storage
  - 4.3 Instance Lifecycle Management
  - 4.4 Message Routing and Delivery
  - 4.5 Terminal Integration and PTY Wrapper
- 5 Command Reference
  - 5.1 Launch and Instance Management
  - 5.2 Messaging Commands
  - 5.3 Events and Query Commands
  - 5.4 Configuration and Management Commands
  - 5.5 Terminal Commands
- 6 Configuration System
  - 6.1 Configuration Files and Precedence
  - 6.2 Terminal Presets
  - 6.3 Settings Reference
- 7 Tool Integration
  - 7.1 Hook System Overview
  - 7.2 Identity and Session Binding
  - 7.3 Claude Code Integration
  - 7.4 PTY Delivery Testing and Validation
- 8 Multi-Agent Communication
  - 8.1 Message Scopes and Delivery Logic
  - 8.2 Event Subscriptions
  - 8.3 Bundles and Context Sharing
  - 8.4 Instance Forking and Subagents
- 9 Cross-Device Synchronization
  - 9.1 Relay Architecture
  - 9.2 Relay Setup and Operations
- 10 Build and Distribution
  - 10.1 Build System and CI/CD
  - 10.2 Installation Methods
- 11 Database Schema Reference
- 12 Development Guide
  - 12.1 Project Structure
  - 12.2 Adding New Commands
  - 12.3 Extending Terminal Support
  - 12.4 Testing and CI
- 13 Agent Skills and Workflow Patterns
  - 13.1 Agent Messaging Skill
  - 13.2 Workflow Scripting and hcom run
- 14 Glossary

## README / Docs Rewrite Source Material

### 1. Product Positioning

- `hcom` is a Rust CLI that connects AI coding agents running in separate terminal sessions. DeepWiki describes it as a shared coordination layer for Claude Code, Gemini CLI, Codex, and OpenCode.
- The main value proposition: agents can message each other, observe activity, launch/fork/resume peers, share transcripts/context bundles, and coordinate without a centralized SaaS service.
- The local system of record is SQLite. DeepWiki repeatedly frames the database as the coordination hub for events, instances, status, message cursors, session bindings, process bindings, subscriptions, and relay state.
- Main implementation references:
  - CLI entry: `src/main.rs` and `src/router.rs`.
  - Commands: `src/commands/`.
  - Database: `src/db/mod.rs`.
  - PTY and delivery: `src/pty/mod.rs`, `src/pty/delivery.rs`.
  - Terminal presets: `src/terminal.rs`, `src/shared/terminal_presets.rs`.
  - Relay: `src/relay/`.
  - Bundled workflow skill/scripts: `skills/hcom-agent-messaging/`, `src/scripts/bundled/`.
- DeepWiki source refs:
  - README purpose and supported tools: `README.md:8-12`.
  - Package metadata: `Cargo.toml:1-14`, `Cargo.toml:6`.
  - CLI entry: `src/main.rs:1-15`.
  - PTY/dependencies: `Cargo.toml:18-19`.
  - Test-backed PTY behavior: `tests/test_pty_delivery.rs:61-70`, `tests/test_pty_delivery.rs:137-147`.

### 2. Quick Start / Installation

- Public install methods:
  - Homebrew: `brew install aannoo/hcom/hcom`.
  - Install script: `curl -fsSL https://github.com/aannoo/hcom/releases/latest/download/hcom-installer.sh | sh`.
  - Python tool/package path: `uv tool install hcom` or `pip install hcom`.
  - Source build: `git clone https://github.com/aannoo/hcom.git && cd hcom && cargo build --release`.
- The old `install.sh` is a compatibility shim that redirects to `hcom-installer.sh`.
- The install script detects Linux, macOS, WSL, and Android/Termux targets and installs a prebuilt binary into a PATH directory.
- Source build requires the Rust toolchain; DeepWiki notes Cargo 1.86+ from `Cargo.toml:10`.
- Verification commands to document:
  - `hcom status` for installation and diagnostic checks.
  - `hcom list` to show active agents and states.
  - `hcom term <name> --json` to inspect a terminal screen.
  - `hcom config terminal --info` to inspect terminal preset behavior.
  - `hcom events --agent <name>` to inspect an agent event log.
  - `hcom relay status` when relay is configured.
- DeepWiki source refs:
  - README install snippets: `README.md:19-35`.
  - Installer shim: `install.sh:1-19`.
  - Android/Termux release workflow handling: `.github/workflows/release.yml:130-150`.
  - Python packaging: `pyproject.toml:1-36`.

### 3. Core Data Model

- Instance:
  - A named participant in the hcom system. Names are unique and often drawn from a curated short-name pool.
  - Stored in the `instances` table and represented by `InstanceRow`.
  - Important fields include `name`, `session_id`, `status`, `last_event_id`, `last_stop`, `running_tasks`, tag, terminal preset fields, and remote-device metadata.
  - DeepWiki refs: `src/db/mod.rs:94-187`, `src/instance_names.rs:7-8`, `src/instance_names.rs:14-36`, `src/instance_names.rs:145-161`.
- Session:
  - A specific execution run of a tool. An instance can have a session binding to a tool session id and a process binding to the wrapper/process identity.
  - DeepWiki refs: `src/db/mod.rs:1566-1773`, `src/instance_binding.rs:1-206`.
- Event:
  - The immutable log unit for message, status, lifecycle, control, and relay result activity.
  - Stored in `events`; queried through `events_v`.
  - DeepWiki refs: `src/db/mod.rs:277-417`, `src/db/mod.rs:681-771`, `src/commands/events.rs:121-185`.
- Message:
  - A message is an event with scope, mentions, intent, optional thread, optional reply target, and text/body.
  - Scopes: `broadcast` or `mentions`.
  - Intents: `request`, `inform`, `ack`.
  - DeepWiki refs: `src/messages.rs:1-80`, `src/messages.rs:15-18`, `src/messages.rs:43-47`, `src/commands/send.rs:52-135`.
- Bundle:
  - A structured context package containing references to events, files, and transcripts, used for handoffs.
  - DeepWiki refs: `src/commands/bundle.rs`, `src/commands/send.rs:106-130`, `src/commands/send.rs:197-201`.

### 4. Command Families

Representative commands below are for docs examples, not an exhaustive parser specification.

| Family | Representative commands | Semantics / notes |
| --- | --- | --- |
| Dashboard | `hcom` | Opens the TUI dashboard when run with no command. |
| Launch | `hcom claude`, `hcom gemini`, `hcom codex`, `hcom opencode`, `hcom 2 claude`, `hcom 1 claude --tag worker --go --headless` | Launches one or more wrapped tool instances. Shared flags reported by DeepWiki include `--tag`, `--terminal`, `--dir`, `--headless`, `--device`, `--hcom-prompt`, and `--hcom-system-prompt`. |
| List / status | `hcom list`, `hcom list --json`, `hcom list --names`, `hcom status` | Shows instances, unread counts, install health, tool state, hcom directory/config validity, and version details. |
| Messaging | `hcom send @luna -- hello`, `hcom send @luna @nova --intent request --thread triage-1 -- Can you help?`, `hcom send @api- --file brief.md`, `hcom send @luna:BOXE -- remote hello` | Sends direct, tag-prefix, broadcast, or remote messages. Supports `--intent`, `--reply-to`, `--thread`, `--file`, and `--base64`. DeepWiki notes `--` separates flags from raw message text. |
| Listen / events | `hcom listen`, `hcom events --agent luna`, `hcom events --last 5`, `hcom events --wait 120 --sql "msg_thread='triage-1'"`, `hcom events sub --idle peso`, `hcom events sub --file '*.py' --once` | Queries and waits on the event stream; subscriptions can fire follow-up messages when filters match. |
| Transcript | `hcom transcript @planner --full`, `hcom transcript luna 10-30`, `hcom transcript luna --last 20` | Reads another agent's conversation or transcript ranges for review and handoff. DeepWiki describes normal/full/detailed levels in bundle transcript references. |
| Terminal | `hcom term luna --json`, `hcom term inject luna 'status?' --enter`, `hcom config terminal --info` | Inspects or writes to the PTY screen. Test refs cover JSON screen inspection and ready-pattern delivery. |
| Lifecycle | `hcom kill luna`, `hcom kill tag:worker`, `hcom kill all`, `hcom stop luna`, `hcom start --as luna`, `hcom start --orphan <name-or-pid>` | Kills, disconnects, joins, reclaims, or recovers hcom participation. |
| Resume / fork | `hcom r luna`, `hcom resume luna`, `hcom f luna`, `hcom fork luna` | Resumes a stopped session or forks a session for parallel work. DeepWiki says fork is supported for Claude, Codex, and OpenCode. |
| Bundles | `hcom bundle list`, `hcom bundle show <id>`, `hcom bundle cat <id>`, `hcom bundle chain <id>`, `hcom bundle prepare`, `hcom bundle create ...` | Creates and expands context packages for handoffs. Inline bundle creation can be attached to sends with `--title`, `--description`, `--events`, `--files`, `--transcript`, and `--extends`. |
| Config | `hcom config`, `hcom config <key>`, `hcom config <key> <value>`, `hcom config -i luna <key> <value>`, `hcom config --edit`, `hcom config --reset`, `hcom config --setup` | Reads, writes, explains, edits, resets, or configures global/per-instance settings. |
| Hooks | `hcom hooks add <tool>`, `hcom hooks status`, `hcom hooks remove <tool>` | Installs/removes/checks tool hook integration. Exact supported subcommands should be verified in local parser before publishing. |
| Relay | `hcom relay new`, `hcom relay connect <token>`, `hcom relay status`, `hcom relay off --all`, `hcom relay push`, `hcom relay daemon start` | Creates/joins relay groups, reports relay health, pushes state, and controls the relay worker daemon. |
| Workflow scripts | `hcom run`, `hcom run debate`, `hcom run <user-script> [args]` | Runs bundled or user scripts. User scripts in `~/.hcom/scripts/` shadow bundled scripts with the same name. |
| Maintenance | `hcom update`, `hcom reset` | Update detection and environment reset/archival behavior. Treat reset as destructive in docs and verify exact prompt/archival semantics before publishing. |

Command implementation references:

- Router and known commands: `src/router.rs`, `dispatch_native_command`, `COMMANDS`.
- Help text: `src/commands/help.rs`.
- Individual command modules: `src/commands/send.rs`, `src/commands/events.rs`, `src/commands/bundle.rs`, `src/commands/run.rs`, `src/commands/resume.rs`, `src/commands/update.rs`.
- Identity resolution from CLI/env/context: explicit `--name`, `HCOM_PROCESS_ID`, then tool-specific variables such as `CLAUDE_SESSION_ID` or directory context.

### 5. Runtime Files and Configuration

- `HCOM_DIR` controls the base runtime directory. If unset, DeepWiki says it defaults to `~/.hcom`.
- Main database: `$HCOM_DIR/hcom.db`.
- Main config: `$HCOM_DIR/config.toml`.
- Legacy config: `$HCOM_DIR/config.env`. DeepWiki says its presence prevents automatic migration to `config.toml` to avoid silent loss of settings.
- Environment file: `$HCOM_DIR/env`, used for non-HCOM environment variables passed to agent processes.
- Runtime directories reported by DeepWiki:
  - `$HCOM_DIR/.tmp/logs` for logs such as `hcom.log`.
  - `$HCOM_DIR/.tmp/launch` for launch wrapper scripts.
  - `$HCOM_DIR/.tmp/flags` for counters/flags.
  - `$HCOM_DIR/launches` for launch history.
  - `$HCOM_DIR/archive` for archived sessions.
  - `$HCOM_DIR/scripts` for user workflow scripts.
  - `$HCOM_DIR/.tmp/launched_pids.json` for PID tracking.
- Config precedence reported by DeepWiki, low to high:
  1. Built-in defaults from `HcomConfig::default()`.
  2. `config.toml`.
  3. Legacy `config.env`.
  4. `HCOM_*` environment variables.
  5. CLI flags.
- `Config` is described as runtime environment configuration for `HCOM_DIR`, `HCOM_INSTANCE_NAME`, and `HCOM_PROCESS_ID`.
- `HcomConfig` is described as user-facing settings with validation and dynamic precedence.
- Per-instance overrides are stored in the database and can override global config for runtime behavior. DeepWiki specifically mentioned `tag`, `timeout`, `hints`, and `subagent_timeout`.
- Relay secrets are file-only:
  - DeepWiki says relay fields such as `relay`, `relay_id`, `relay_token`, `relay_psk`, and `relay_enabled` are restricted away from environment propagation, with `relay_psk` excluded from `FIELD_TO_ENV`.
- Terminal presets:
  - Built-ins come from `TERMINAL_PRESETS`.
  - Custom TOML presets live under `[terminal.presets.NAME]`.
  - Resolution merges built-in definitions with config customization.
  - DeepWiki says terminal names are validated against dangerous shell characters to reduce command injection risk.
- DeepWiki source refs:
  - Database open/init/logging: `src/db/mod.rs:205-227`, `src/db/mod.rs:277-417`, `src/db/mod.rs:681-771`.
  - Terminal preset internals: `src/terminal.rs:32-41`, `src/shared/terminal_presets.rs:7-20`.
  - Run script discovery: `src/commands/run.rs:1-5`, `src/commands/run.rs:112-115`, `src/commands/run.rs:150-160`.

### 6. Hook Internals and Session Binding

- Supported tools reported by DeepWiki:
  - Claude Code.
  - Gemini CLI.
  - Codex.
  - OpenCode.
- Hook events reported by DeepWiki:
  - Claude Code: `sessionstart`, `userpromptsubmit`, `pre`, `post`, `poll`, `notify`, `permission-request`, `subagent-start`, `subagent-stop`, `sessionend`.
  - Gemini CLI: `gemini-sessionstart`, `gemini-aftertool`.
  - Codex: `codex-sessionstart`, `codex-userpromptsubmit`, `codex-pretooluse`, `codex-posttooluse`, `codex-stop`.
  - OpenCode: `opencode-start`, `opencode-status`, `opencode-read`, `opencode-stop`.
- Hook installation:
  - DeepWiki describes `hcom hooks add [tool]` as injecting `hcom` as the handler into the target tool's config.
  - `hcom hooks status` verifies installation.
  - Exact target config file paths were not fully detailed in the public answers and should be verified in local source before publishing.
- Hook dispatch:
  - `dispatch_claude_hook` reads JSON from stdin, builds context, and routes events to handlers.
  - Hook execution is wrapped by a panic guard so hook failure does not crash the host AI tool.
  - DeepWiki refs: `src/hooks/claude.rs:31-40`, `src/hooks/claude.rs:47-118`.
- Hook gating:
  - `hook_gate_check` avoids expensive work and interference for processes not participating in hcom.
  - It checks whether a process was hcom-launched via context/process id or whether active instances exist.
  - DeepWiki refs: `src/hooks/claude.rs:89-92`.
- Binding tables:
  - `session_bindings`: maps tool `session_id` to hcom `instance_name`; primary gate for hook participation.
  - `process_bindings`: maps process identity to session and instance; used by hcom-launched PTY wrappers and `HCOM_PROCESS_ID`.
  - DeepWiki refs: `src/db/mod.rs:1566-1773`, `src/instance_binding.rs:201-206`.
- Startup/bootstrap:
  - During `sessionstart`, hcom-launched instances bind tool sessions to process identities and receive bootstrap instructions.
  - Vanilla, non-hcom-launched sessions may receive a hint to run `hcom start` to participate.
  - DeepWiki refs: `src/bootstrap.rs:19-28`, `src/bootstrap.rs:111-146`, `src/bootstrap.rs:159-165`.
- Status and heartbeat:
  - Important states: `active`, `listening`, `blocked`, `launching`, `inactive`, `stopped`.
  - Lifecycle actions include `created`, `started`, `ready`, `stopped`.
  - DeepWiki reports TCP heartbeat threshold as 35 seconds and ad-hoc/no-TCP as 10 seconds.
  - DeepWiki refs: `src/instance_lifecycle.rs:20-40`, `src/instance_lifecycle.rs:77-154`, `src/instance_lifecycle.rs:157-331`, `src/instance_lifecycle.rs:350-440`, `src/db/mod.rs:1443-1467`, `src/shared/constants.rs:81-110`.
- Subagents:
  - Claude subagent hooks track hierarchy.
  - DeepWiki says hcom eagerly allocates an instance row when a subagent starts so it can be targeted by `hcom send`.
  - `running_tasks` tracks active subagents/tasks in instance state.

### 7. Message Delivery and Subscriptions

- Sending:
  - `hcom send` writes a message event to SQLite.
  - PTY delivery and/or hooks pick up unread messages and inject or surface them when safe.
  - DeepWiki refs: `src/commands/send.rs:52-135`, `src/db/mod.rs:788-814`, `src/pty/delivery.rs`.
- Delivery rules:
  - `should_deliver_to` skips messages sent by the receiver itself.
  - `broadcast` messages are delivered to active participants except sender.
  - `mentions` messages go only to named mentions.
  - DeepWiki says mention matching uses base-name matching so remote suffixes do not alter stored scope.
  - DeepWiki says `has_direct_unread` ignores broadcasts to avoid waking dormant subagents unnecessarily.
- Message metadata:
  - `--intent request` expects a response and can create request-watch behavior.
  - `--intent inform` is informational.
  - `--intent ack` acknowledges a request and requires `--reply-to`.
  - DeepWiki says the system prevents ack-on-ack loops and acknowledgements of informational messages.
  - `--thread` groups messages and can reuse seeded recipients across subsequent messages in the same thread.
  - `--reply-to` can address local and remote event ids, e.g. `42` or `42:BOXE`.
- Unread cursors:
  - Instances maintain `last_event_id`.
  - Delivery consumes from the event stream and advances cursors.
  - DeepWiki refs: `src/db/mod.rs:147`, `src/db/events.rs`.
- Subscriptions:
  - Subscriptions are stored in `kv` with prefix `events_sub:`.
  - `create_filter_subscription` builds SQL predicates from CLI flags and stores JSON.
  - When events are logged, subscriptions are checked against `events_v`; matches can trigger `on_hit_text`.
  - Special cases include request-watch subscriptions, collision detection, and thread membership.
  - Request-watch keys were reported as `reqwatch-{id}-{recipient}`.
  - Collision detection watches file-write status events, especially `tool:Write`, to warn about concurrent edits.
  - DeepWiki refs: `src/db/subscriptions.rs:182-205`, `src/commands/events.rs`.
- PTY delivery:
  - Uses ready patterns to avoid injecting while tools are not ready. Examples from tests include Claude `? for shortcuts` and Gemini `Type your message`.
  - Gate blocking prevents delivery when the prompt already has uncommitted user text.
  - DeepWiki refs: `tests/test_pty_delivery.rs:13-15`, `tests/test_pty_delivery.rs:61-70`, `tests/test_pty_delivery.rs:137-147`, `tests/test_pty_delivery.rs:149-161`.

### 8. Bundles, Transcripts, and Workflow Scripts

- Bundles:
  - Purpose: package context for handoffs without dumping raw terminal state into every message.
  - Reference types: events, files, and transcript ranges.
  - Commands: `bundle list`, `bundle show`, `bundle cat`, `bundle chain`, `bundle prepare`, `bundle create`.
  - Inline send creation: `hcom send` can build and attach a bundle using flags such as `--title`, `--description`, `--events`, `--files`, `--transcript`, and `--extends`.
  - `bundle cat` expands the referenced content; `bundle show` displays metadata; `bundle chain` shows lineage; `bundle prepare` suggests a template from recent context.
  - DeepWiki refs: `src/commands/bundle.rs`, `src/commands/send.rs:106-130`, `src/commands/send.rs:197-201`.
- Transcripts:
  - `hcom transcript` lets an agent or reviewer read another agent's conversation.
  - Bundles can include transcript ranges at normal, full, or detailed levels.
  - Workflow scripts use transcript reads for reviewer and pipeline patterns, e.g. `hcom transcript @planner --full`.
- `hcom run`:
  - Runs bundled scripts compiled into the binary and user scripts from `~/.hcom/scripts/`.
  - User scripts shadow bundled scripts with the same name.
  - Metadata extraction:
    - Shell scripts: first comment after shebang.
    - Python scripts: first line of module docstring.
  - `hcom run` injects `--name` so scripts can propagate orchestrator identity to launched agents.
  - DeepWiki refs: `src/commands/run.rs:1-5`, `src/commands/run.rs:49-104`, `src/commands/run.rs:111-163`, `src/commands/run.rs:223-255`.
- Skill/workflow patterns surfaced by DeepWiki:
  - Basic messaging: launch agents, send direct messages, verify events.
  - Worker-reviewer: worker sends `ROUND DONE`; reviewer inspects transcript and sends fix/approval.
  - Ensemble consensus: independent agents respond to a shared thread; judge aggregates from `hcom events --sql`.
  - Sequential cascade: planner emits `PLAN DONE`; executor waits on `hcom events --wait` and reads planner transcript.
  - Hub-spoke/fatcow: persistent or resumable oracle pattern.
- Workflow script safety notes:
  - Always capture launched names from `Names: ...` output.
  - Use unique `--thread` values to isolate concurrent runs.
  - Use `trap` cleanup to kill tracked agents on script failure.
  - Use `--go` for unattended scripts to avoid TTY confirmation hangs.
  - Wait until agents reach `active` or `listening` before sending messages, especially for tools whose session binding may happen after first tool activity.
  - DeepWiki refs: `skills/hcom-agent-messaging/references/script-template.md:21-98`, `skills/hcom-agent-messaging/references/gotchas.md:1-160`, `src/commands/help.rs:50-53`.

### 9. Forking, Resume, and Remote Control

- Resume:
  - `hcom r <target>` or `hcom resume <target>` resumes stopped agents.
  - Target can be a name, session UUID, or thread name according to DeepWiki's command semantics answer.
- Fork:
  - `hcom f <target>` or `hcom fork <target>` forks an active or stopped session.
  - DeepWiki says supported tools include Claude, Codex, and OpenCode.
- Implementation:
  - DeepWiki points to `src/commands/resume.rs` and `do_resume` for both resume and fork.
  - It handles local and remote operations and can preview the plan.
  - DeepWiki notes remote forks require `--dir` for the working directory on the target device.
- Subagent launches:
  - `hcom 1 claude --tag cool` is the representative launch form for named/tagged subagent workers.
  - Subagents can be addressed by name or tag prefix such as `@cool-`.

### 10. Relay and Cross-Device Sync

- Relay architecture:
  - Cross-device sync is implemented over MQTT.
  - Each device runs a relay worker daemon.
  - A shared `relay_id` groups trusted devices.
  - Each device has a unique `device_uuid`; a short id is used for display/logging.
- MQTT topics reported by DeepWiki:
  - `{relay_id}/{device_uuid}`: retained per-device state snapshots.
  - `{relay_id}/control`: non-retained cross-device RPC control commands.
  - `{relay_id}/+`: wildcard subscription used to monitor peer devices.
- Sync behavior:
  - Local state and event batches are pushed.
  - Incoming state messages are decrypted, checked against watermarks, and used to upsert remote instances/events.
  - Remote instances appear locally with suffixes like `luna:BOXE`.
  - Remote commands use request/response RPC over control topic and write results as `rpc_result` events.
- Relay CLI:
  - `hcom relay new`: creates a new relay group, generates `relay_id` and PSK, tests broker connectivity.
  - `hcom relay connect <token>`: joins an existing relay group. DeepWiki says token contains relay id, broker URL, and raw PSK.
  - `hcom relay status`: displays health and devices.
  - `hcom relay off [--all]`: disables relay locally and can notify peers.
  - `hcom relay push`: immediate push.
  - `hcom relay daemon [start|stop|restart]`: relay worker process management.
- Relay security:
  - DeepWiki says all payloads use XChaCha20-Poly1305 symmetric AEAD encryption.
  - PSK is a 32-byte pre-shared key stored in `~/.hcom/config.toml` with `0600` permissions.
  - Encryption envelope includes a suite byte, 24-byte nonce, 8-byte timestamp, and ciphertext with 16-byte Poly1305 tag.
  - Associated data binds `relay_id`, topic, and timestamp.
  - Replay protection:
    - timestamp skew window, reported as 60 seconds.
    - LRU of `(sender, nonce)` pairs.
  - `HCOM_RELAY_TOKEN` is broker authentication and distinct from the end-to-end PSK.
  - DeepWiki says legacy tokens without PSK are rejected.
- Threat model and limitations:
  - Relay is a single trust domain for one operator's devices.
  - A leaked relay token/PSK gives full decryption and publishing ability for that relay group.
  - A compromised member can send text to agents and use remote RPC; if agents can run tools, impact can approach shell access.
  - Broker/network observers can see metadata: topic names, timing, sizes, connection patterns.
  - No forward secrecy; captured encrypted traffic can be decrypted later if the PSK leaks.
  - No scoped per-device roles or granular authorization.
  - Local OS compromise is out of scope.
  - Incident response: `hcom relay off --all` can notify reachable peers, but PSK cannot be revoked; create a new relay group with `hcom relay new`.
- DeepWiki source refs:
  - Relay overview/security in README: `README.md:121-150`.
  - Crypto dependency: `Cargo.toml:47`.
  - Control topic and relay ids: `src/relay/control.rs:1-4`, `src/relay/control.rs:34-45`.
  - Control payload/RPC flow: `src/relay/control.rs:23-75`, `src/relay/control.rs:145-168`, `src/relay/control.rs:194-207`.
  - Client/push/pull/crypto/replay modules: `src/relay/client.rs`, `src/relay/push.rs`, `src/relay/pull.rs`, `src/relay/crypto.rs`, `src/relay/replay.rs`.

### 11. Build, Test, Packaging, and Release

- Project shape:
  - Rust binary crate named `hcom`.
  - Python packaging uses `maturin` so the Rust binary can be distributed through Python wheels.
  - DeepWiki source refs: `Cargo.toml:1-14`, `pyproject.toml:1-36`.
- Important dependencies:
  - `clap` for CLI parsing.
  - `rusqlite` for SQLite.
  - `serde` for serialization.
  - `nix` for Unix/PTY system calls.
  - `vt100` for terminal screen parsing.
  - `ratatui` and `crossterm` for the TUI.
  - `rumqttc` for MQTT relay.
  - `chacha20poly1305` / XChaCha20-Poly1305-related crypto for relay payload protection.
- CI/release workflows reported by DeepWiki:
  - `ci.yml`: runs tests on push/pull request.
  - `build-wheels.yml`: builds Python wheels for multiple targets, including Android/Termux handling.
  - `release.yml`: tag-triggered release workflow using `cargo-dist`, generates installers and GitHub release artifacts, and publishes Homebrew formulae.
  - `publish-pypi.yml`: publishes standard wheels to PyPI; DeepWiki says Android-compatible wheels are uploaded to GitHub Release assets.
- Supported platforms/targets:
  - Linux x86_64 and aarch64, GNU/MUSL variants.
  - macOS x86_64 and aarch64.
  - Android/Termux aarch64.
  - DeepWiki says `dist-workspace.toml` lists target triples and `cargo-dist-version = "0.31.0"`.
- Verification commands for local repo docs:
  - `cargo fmt --all -- --check`.
  - `cargo clippy --all-targets --locked -- -D warnings`.
  - `cargo test --locked`.
  - The upstream public wiki only says `cargo test`; this fork's project instructions require the locked/fmt/clippy forms above.
- Update behavior:
  - `hcom update` compares compiled `CARGO_PKG_VERSION` to latest remote tag.
  - DeepWiki says it detects installation method such as Homebrew, uv, pip, or curl installer and gives the appropriate update command.
  - DeepWiki refs: `src/update.rs`, `src/commands/update.rs`.

## Suggested Rewrite Structure

Use this as the source outline for README/docs/skill rewriting:

1. Opening README section:
   - "hcom connects isolated AI coding agents across terminals."
   - Include one concrete two-terminal example before architectural detail.
2. Install:
   - Prefer Homebrew/install script/uv/source build.
   - Add `hcom status` and `hcom list` verification immediately after installation.
3. First run:
   - `hcom claude` in one terminal, `hcom codex` or `hcom gemini` in another.
   - `hcom send @name -- ...`.
   - `hcom events --last 5` for confirmation.
4. Core concepts:
   - Instances, sessions, events, messages, bundles, relay.
   - Make SQLite the mental model, not the first sales pitch.
5. Command reference:
   - Group by task: launch, message, observe, share context, coordinate workflows, relay, configure, maintain.
6. Configuration/runtime:
   - Explain `HCOM_DIR` and what is safe/unsafe to delete/reset.
   - Separate durable config from transient `.tmp` state.
7. Hooks:
   - Explain the hook gate and binding model at a high level.
   - Include "vanilla sessions are not automatically participants; use `hcom start`."
8. Workflow skill:
   - Lead with safe script patterns: capture names, use tags/threads, wait for readiness, trap cleanup, use `--go`.
   - Keep examples copy/pasteable and tool-agnostic where possible.
9. Relay:
   - Put the trust model near setup commands, not buried after them.
   - State that relay membership is full trust and PSK leakage requires a new relay group.
10. Development:
   - Include build/test/package commands and where to add commands, terminal presets, hooks, and tests.

## Uncertainty and Verification Needs

- Public DeepWiki answers are generated from upstream `aannoo/hcom`; this local repository is a fork and may contain diverging behavior. Before final README/docs/skill edits, verify command flags against the local parser/help output.
- `read_wiki_contents` was called as the public tool supports it: `repoName` only. The public MCP did not expose an all-pages flag or section selector. It returned wiki contents for the repository, but the conversation display truncated some page text.
- Some DeepWiki answers provided module names without exact line references, especially for configuration internals, relay crypto/replay, hook installation target paths, and reset/update behavior.
- DeepWiki sometimes references `src/db.rs` in glossary-like pages and `src/db/mod.rs` in core pages. Treat these as generated references and verify actual paths in the local fork before publishing.
- Exact hook installation paths for Claude, Gemini, Codex, and OpenCode were not fully specified in the public answers.
- Exact destructive behavior of `hcom reset` should be verified before documenting; docs should call it destructive/archival until source confirms prompt and backup behavior.
- Remote fork/launch semantics and required `--dir` behavior should be verified against current local source before final docs.
- Relay security claims should be checked against `src/relay/crypto.rs`, `src/relay/replay.rs`, and config write permissions in local source before publishing user-facing security guarantees.
- The command family table is representative. Do not copy it as an exhaustive command reference without checking `hcom help`, `src/router.rs`, and `src/commands/help.rs`.
