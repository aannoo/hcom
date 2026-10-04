# Contributing

Issues and pull requests are welcome. Thanks for contributing!

## Development

Prerequisites: Rust 1.88+. See pinned [Rust toolchain](rust-toolchain.toml).

```bash
git clone https://github.com/aannoo/hcom.git
cd hcom
cargo build
cargo test
```

### Using a local build

**Symlink** — simple, dev build is global.

```bash
ln -sf "$(pwd)/target/debug/hcom" ~/.cargo/bin/hcom
```

**dev_root** — works regardless of how hcom was installed (brew, pip, etc.); picks the newer of debug/release automatically:

```bash
hcom config dev_root "$(pwd)"
hcom config dev_root --unset  # revert
hcom status                   # run local build
```

For concurrent worktrees, scope each to its own DB:

```bash
HCOM_DIR=$PWD/.hcom HCOM_DEV_ROOT=$PWD hcom claude
```

## CI

Run the full CI gate locally:

```bash
just ci
```

Runs: release-workflow check, fmt, clippy, typecheck, msrv, unit tests, integration tests, and live relay test. See [justfile](justfile) for details.

`just ci` integration tests run genuine Codex/Claude binaries against local mock providers. No model/API usage.

## Debugging

```bash
hcom status --logs
hcom term debug --help # access pty log file
ls -a ~/.hcom/ ~/.hcom/.tmp/ # config, env, db, integrations, logs, launch scripts
```

## Pull requests

- Live test with the real tool
- Prefer preserving the underlying agent behavior
- Prefer shared behavior across tools
- Support latest agent version
