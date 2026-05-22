# Configuration

`hcom` has two configuration layers:

- `Config`: startup/runtime values such as `HCOM_DIR`, `HCOM_INSTANCE_NAME`, and `HCOM_PROCESS_ID`.
- `HcomConfig`: user-facing settings from `config.toml`, environment variables, and defaults.

## HCOM_DIR

`HCOM_DIR` is the base directory for hcom runtime state.

Default:

```text
~/.hcom
```

Project-local mode:

```bash
export HCOM_DIR="$PWD/.hcom"
```

Relative `HCOM_DIR` values are resolved against the current working directory. `~` expands against `HOME` or `USERPROFILE`.

Avoid putting `HCOM_DIR` under protected metadata directories such as `.git`, `.codex`, `.claude`, or `.agents`; tool sandboxes may block writes there.

## Files and Directories

| Path | Purpose |
|---|---|
| `$HCOM_DIR/hcom.db` | SQLite database |
| `$HCOM_DIR/config.toml` | primary config |
| `$HCOM_DIR/config.env` | legacy config |
| `$HCOM_DIR/env` | non-HCOM environment passed to launched agents |
| `$HCOM_DIR/.tmp/logs/hcom.log` | runtime log |
| `$HCOM_DIR/.tmp/launch/` | launch wrapper scripts |
| `$HCOM_DIR/.tmp/flags/` | counters and transient flags |
| `$HCOM_DIR/.tmp/launched_pids.json` | PID tracking |
| `$HCOM_DIR/launches/` | launch history |
| `$HCOM_DIR/archive/` | archived sessions |
| `$HCOM_DIR/scripts/` | user workflow scripts |

## Precedence

User-facing settings are loaded in this effective order:

```text
defaults < config.toml < HCOM_* environment variables < CLI flags
```

Relay fields are an exception: relay URL, relay ID, relay token, relay PSK, and relay enabled state are file-only when loaded. The relay PSK is deliberately not exported to child process environments.

## Config Keys

Common keys:

| Key | Env var | Purpose |
|---|---|---|
| `terminal` | `HCOM_TERMINAL` | terminal preset or custom command with `{script}` |
| `tag` | `HCOM_TAG` | group/label for launched agents |
| `hints` | `HCOM_HINTS` | appended to received messages |
| `notes` | `HCOM_NOTES` | appended to bootstrap |
| `subagent_timeout` | `HCOM_SUBAGENT_TIMEOUT` | subagent keep-alive seconds |
| `claude_args` | `HCOM_CLAUDE_ARGS` | default Claude args |
| `gemini_args` | `HCOM_GEMINI_ARGS` | default Gemini args |
| `codex_args` | `HCOM_CODEX_ARGS` | default Codex args |
| `codex_sandbox_mode` | `HCOM_CODEX_SANDBOX_MODE` | Codex sandbox mode |
| `opencode_args` | `HCOM_OPENCODE_ARGS` | default OpenCode args |
| `timeout` | `HCOM_TIMEOUT` | default wait/listen timeout |
| `auto_approve` | `HCOM_AUTO_APPROVE` | install permission rules for safe hcom commands |
| `auto_subscribe` | `HCOM_AUTO_SUBSCRIBE` | auto-subscribe presets |
| `name_export` | `HCOM_NAME_EXPORT` | export agent name to custom env var |

Relay keys:

| Key | Notes |
|---|---|
| `relay` | broker URL |
| `relay_id` | relay group ID |
| `relay_token` | broker authentication token/password |
| `relay_psk` | 32-byte relay PSK, file-only |
| `relay_enabled` | relay on/off state |

## Validation

hcom validates:

- `timeout` and `subagent_timeout`: 1 to 86400 seconds.
- `tag`: letters, numbers, and hyphens.
- terminal preset names and custom terminal commands.
- shell-quoted tool args.
- Codex sandbox mode: `workspace`, `untrusted`, `danger-full-access`, or `none`.
- Codex launch args: hidden Codex `--yolo` is accepted and treated as a sandbox override.
- `auto_subscribe`: comma-separated alphanumeric/underscore preset names.

## Terminal Presets

Terminal values can be:

- `default`
- `print`
- `here`
- a built-in preset such as `tmux`, `wezterm`, or `kitty` when available
- a user-defined preset under `[terminal.presets.NAME]`
- a custom command containing `{script}`

Inspect terminal behavior:

```bash
hcom config terminal --info
```

## Per-Agent Config

Per-agent config is stored in the database and can override global behavior for a specific instance:

```bash
hcom config -i self
hcom config -i luna hints "Focus on tests first."
hcom config -i review-luna subagent_timeout 120
```

Supported per-agent keys include `tag`, `timeout`, `hints`, and `subagent_timeout`.

## Reset Behavior

```bash
hcom reset
hcom reset hooks
hcom reset all
```

`hcom reset` stops local instances, stops the relay daemon if running, cleans temp files, archives and clears the database, creates a fresh database, and triggers relay sync.

`hcom reset hooks` removes hcom hooks.

`hcom reset all` additionally clears PID tracking, removes hooks, archives/deletes config files, and clears device identity.

Inside AI tools, reset prints a preview unless `--go` is provided. Treat reset commands as destructive even though the active database is archived.
