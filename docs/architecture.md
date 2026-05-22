# Architecture

`hcom` is a local coordination layer for AI coding agents. The core design is intentionally simple: hooks and PTY wrappers write facts to SQLite, and delivery code reads pending messages back out when a target agent is ready.

```text
tool session -> hook/PTy wrapper -> SQLite -> hook/PTy delivery -> target session
```

## Main Modules

| Area | Files |
|---|---|
| CLI routing | `src/main.rs`, `src/router.rs`, `src/commands/` |
| Config and paths | `src/config.rs`, `src/paths.rs`, `src/runtime_env.rs` |
| Database | `src/db/` |
| Identity and lifecycle | `src/identity.rs`, `src/instance_binding.rs`, `src/instance_lifecycle.rs`, `src/instances.rs`, `src/instance_names.rs` |
| Messages and delivery | `src/messages.rs`, `src/delivery.rs`, `src/hooks/common.rs`, `src/pty/` |
| Tool hooks | `src/hooks/claude.rs`, `src/hooks/codex.rs`, `src/hooks/gemini.rs`, `src/hooks/opencode.rs` |
| Relay | `src/relay/` |
| TUI | `src/tui/` |
| Transcripts | `src/transcript/` |
| Workflow scripts | `src/commands/run.rs`, `src/scripts/bundled/`, `skills/hcom-agent-messaging/` |

## CLI Routing

`src/router.rs` classifies arguments into:

- hook handlers, such as `codex-stop` or `sessionstart`.
- native commands, such as `send`, `events`, `bundle`, and `relay`.
- launch commands, such as `hcom 3 claude`.
- PTY wrapper mode.
- TUI mode when no command is given.

Global flags include `--name` for identity and `--go` for confirmation bypass in actions that would otherwise preview inside AI tools.

## Data Model

The database is stored at `$HCOM_DIR/hcom.db`.

Core records:

- `instances`: active and known agents. Tracks name, status, tool/session metadata, tag, terminal info, relay origin, and event cursor.
- `events`: immutable message/status/lifecycle/control log.
- `events_v`: query view used by `hcom events`, `hcom listen`, and subscriptions.
- `kv`: general state, including event subscriptions and relay metadata.
- `session_bindings`: maps tool session IDs to hcom instance names.
- `process_bindings`: maps wrapper/process IDs to sessions and instances.

Events are the durable audit stream. Instance rows are the current-state index over that stream.

## Identity and Binding

hcom has two participation modes:

- **hcom-launched**: `hcom claude`, `hcom codex`, `hcom gemini`, or `hcom opencode` creates an instance, wraps the process, and exports hcom identity state such as `HCOM_PROCESS_ID`.
- **Ad-hoc**: a tool started normally can run `hcom start` or `hcom start --as <name>` to join.

Hooks resolve the active hcom instance through:

1. explicit `--name` when present.
2. process binding from `HCOM_PROCESS_ID`.
3. session binding from tool session ID.
4. bind markers in transcripts for vanilla sessions that later join.

This is why a tool may appear as pending until its first prompt or hook event binds the tool session to an hcom instance.

## Lifecycle

Common statuses:

- `launching`: process is being started.
- `active`: prompt or tool work is in progress.
- `listening`: ready for messages.
- `blocked`: agent needs human approval or input.
- `inactive` / `stopped`: process ended or was disconnected.

Lifecycle events include `created`, `started`, `ready`, `stopped`, and `batch_launched`.

## Message Delivery

`hcom send` writes message events. Delivery then depends on the target:

- Hook-integrated tools receive pending messages through hook output at safe points.
- PTY-wrapped tools can receive injected text when the screen parser reports that the target is ready and its prompt is empty.
- `hcom listen` lets any process block until a message or event arrives.

Delivery protects against noisy wakeups:

- Senders do not receive their own messages.
- Direct mentions target only matching instances.
- Broadcasts reach active participants but do not wake dormant subagents.
- Message cursors advance after successful delivery.

## PTY Integration

PTY support captures terminal output and can inject text. `hcom term` exposes the current screen:

```bash
hcom term <name>
hcom term <name> --json
hcom term inject <name> "text" --enter
```

The JSON screen includes lines, size, cursor, readiness, whether the prompt is empty, and current input text. PTY tests verify ready-pattern handling and prompt-empty gates.

## Hooks

Hooks are a low-overhead bridge between supported tools and hcom. They:

- record status and tool activity.
- bind sessions to instances.
- deliver pending messages.
- update transcript and cwd metadata.
- track Claude subagent lifecycle.
- avoid noisy output when hcom is installed but not active.

See [Hook System](hook-system.md).

## Relay

Relay syncs trusted devices through MQTT:

- local events and instance state are pushed to broker topics.
- remote retained state is pulled and stored as remote instances.
- RPC-style control messages support remote send/launch/resume/fork operations.
- payloads are encrypted before publishing.

Remote agents appear with suffixes such as `name:BOXE`.

See [Relay](relay.md).
