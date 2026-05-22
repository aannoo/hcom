# Development

hcom is a Rust binary crate with Python packaging through `maturin`.

## Commands

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo run -- <command>
```

The GitHub CI workflow runs format, clippy, and tests on push and pull request.

## Project Layout

| Path | Purpose |
|---|---|
| `src/main.rs` | program entry |
| `src/router.rs` | argv classification and dispatch |
| `src/commands/` | command implementations |
| `src/config.rs` | config loading, validation, precedence |
| `src/paths.rs` | `HCOM_DIR` path resolution |
| `src/db/` | SQLite schema and data access |
| `src/hooks/` | tool integrations |
| `src/pty/` | PTY wrapper, screen parser, injection |
| `src/relay/` | MQTT relay, crypto, replay, RPC |
| `src/tui/` | dashboard |
| `src/transcript/` | transcript readers |
| `src/scripts/bundled/` | bundled workflow scripts |
| `skills/hcom-agent-messaging/` | bundled skill and workflow examples |
| `tests/` | integration and smoke tests |

## Adding a Command

1. Add the command module under `src/commands/`.
2. Register the command in `src/commands/mod.rs`.
3. Add the command name to `COMMANDS` in `src/router.rs`.
4. Add user-facing help in `src/commands/help.rs` when appropriate.
5. Add CLI smoke or parser drift tests.

Keep command parsing consistent with existing modules: clap for command-local parsing, router handling for top-level hcom dispatch.

## Adding Hook Behavior

1. Normalize tool payloads through `HookPayload` where possible.
2. Use the shared hook gate to avoid noisy output for non-participants.
3. Do not crash host tools; hook dispatch should fail quiet and log diagnostics.
4. Keep destructive/admin commands out of safe auto-approved command lists.
5. Add tests for hook config mutation and dispatch behavior.

## Adding Terminal Support

Terminal presets live in `src/shared/terminal_presets.rs` and config resolution is in `src/config.rs` / terminal modules. Validate preset names and avoid shell-injection-prone values.

## Adding Relay Behavior

Relay changes should be treated as security-sensitive:

- keep PSK file-only.
- avoid printing relay tokens or PSKs in routine output.
- preserve AEAD associated-data binding.
- update replay tests when changing envelope or timing behavior.
- document trust-model changes in `docs/relay.md`.

## Packaging

`pyproject.toml` uses `maturin` with `bindings = "bin"`. Release workflows build wheels and GitHub release artifacts. `dist-workspace.toml` controls cargo-dist settings and target triples.

Primary install paths:

- Homebrew formula from upstream release.
- shell installer from GitHub release assets.
- PyPI / `uv tool install`.
- source build with Cargo.

## Documentation Source Discipline

For public upstream repository facts, use hosted public DeepWiki MCP (`mcp__deepwiki__`) and record the call. For local/private filesystem analysis, use DeepWiki Local (`mcp__deepwiki_local__`) and record why local analysis was needed.
