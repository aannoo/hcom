# hcom Fork Instructions

## Project Purpose

This repository is `RichelynScott/hcom`, a fork of `aannoo/hcom`. `hcom` is a Rust CLI for connecting AI coding agents across terminals so they can message, observe, spawn, fork, resume, and coordinate each other.

Current local purpose: keep the fork rebaseable while improving README, reference docs, and the bundled `skills/hcom-agent-messaging` skill for local AIBC multi-agent workflows.

## Commands

- Format check: `cargo fmt --all -- --check`
- Lint: `cargo clippy --all-targets --locked -- -D warnings`
- Test: `cargo test --locked`
- Run CLI locally: `cargo run -- <command>`
- Installed live binary check: `hcom status`
- Project index refresh: `/home/riche/.claude-code-project-index/scripts/project-index-helper.sh`

The CI workflow runs `cargo fmt`, `cargo clippy`, and `cargo test`. Doc-only changes do not require a Rust build. Rust source changes require the Rust toolchain from `rust-toolchain.toml`.

## Architecture

- `src/main.rs` and `src/commands/`: CLI entry points and subcommands.
- `src/hooks/`: tool-specific hook integration for Claude Code, Codex, Gemini, and OpenCode.
- `src/db/`: SQLite-backed event, instance, session, subscription, and KV storage.
- `src/delivery.rs`, `src/router.rs`, `src/messages.rs`: message routing and delivery behavior.
- `src/relay/`: cross-device MQTT relay with encrypted payloads.
- `src/tui/`: terminal UI.
- `src/transcript/`: transcript extraction for supported tools.
- `skills/hcom-agent-messaging/`: bundled agent skill and examples.
- `plugin/` and `.claude-plugin/`: packaged plugin metadata.
- `tests/`: CLI smoke, parser drift, PTY delivery, relay roundtrip, and support fixtures.

## Runtime Ownership

- Live hcom runtime state is under `~/.hcom`, including `config.toml`, `hcom.db`, and logs.
- This repository contains source and docs only; editing it does not change the installed live `hcom` binary.
- Codex project instructions live in this `AGENTS.md`.
- Do not create or modify Claude Code runtime files such as `~/.claude/settings.json` from Codex.
- Do not commit transient local hook logs, generated runtime state, secrets, or local databases.

## Skills And MCP Usage

- Use hosted public `$deepwiki` / `mcp__deepwiki__` for public GitHub repositories and generated wiki extraction. Public endpoint: `https://mcp.deepwiki.com/mcp`.
- Use `$deepwiki-local` / `mcp__deepwiki_local__` only for private repositories, local filesystem paths, offline/container RAG, or explicit local DeepWiki work.
- Do not substitute DeepWiki Local for the hosted DeepWiki MCP when a task references public GitHub docs or the Devin DeepWiki MCP setup page.
- Use `$index` to refresh `PROJECT_INDEX.json`.
- Use `$setup-docs` when repairing Codex project documentation.
- Use `$hcom` when coordinating with other agents through local hcom.
- Treat `/home/riche/.claude/skills` as read-only reference material; Codex-visible skills belong in project `.agents/skills`, `/home/riche/.agents/skills`, or `/home/riche/.codex/skills/.system`.

## Git Rules

- Keep fork customizations scoped so upstream merges remain straightforward.
- Prefer documentation-specific commits for README, docs, skills, and index changes.
- Do not force push `main`.
- Preserve user changes in the working tree; inspect before editing files that are already modified.

## Safety Constraints

- Never commit secrets, tokens, hcom relay PSKs, local SQLite databases, or runtime logs.
- Do not run destructive reset/clean/remove commands without explicit approval.
- Do not run install/uninstall or global hook mutation commands unless that is explicitly requested.
- Before changing OS, security, network, database, infrastructure, or hostile-artifact surfaces, use the required mutation-script review gate.

## Documentation Notes

- Keep `FYI.md` append-only for significant decisions.
- `HANDOFF.md` records session continuity and can be updated at handoff points.
- `PROJECT_INDEX.json` is generated and should be refreshed after large source or documentation changes when useful.
- `docs/deepwiki-mcp-usage.md` records the hosted DeepWiki MCP vs DeepWiki Local boundary for future Codex and Claude sessions.
