# FYI — hcom (fork)

**Created by:** Claude Code (Opus 4.7 1M session)
**Date:** 2026-05-22

Append-only decision journal. Newest entries at the bottom.

---

## 2026-05-22 — Fork cloned into ~/MCPs/
### What
Cloned `github.com/RichelynScott/hcom` (the user's fork of `aannoo/hcom`) into `~/MCPs/hcom`.
### Why
Bring the hcom source under `~/MCPs/` management alongside the other adopted tools (`claude-context-bridge`, `codex-plugin-cc`). Intent: refine hcom's README / docs / skill for our own use.
### How
`git clone` of the fork. Verified version state with `git log` against `upstream/main` (`aannoo/hcom`):
- Fork HEAD == `aannoo/hcom` main == installed `hcom` binary — **all v0.7.17, byte-identical** (0 commits ahead, 0 behind).
- **No reinstall needed** — the installed `hcom` (`uv tool`, `~/.local/bin/hcom`) is already current. The user's expectation of "updates since last version" did not hold; everything is in sync at 0.7.17.
### Impact
hcom source is now local and modifiable. It is a **Rust** project (single binary, built via `maturin`). It ships its own Claude skill (`skills/hcom-agent-messaging/`) and plugin (`plugin/hcom/`). hooks are already installed for Claude + Codex; 5 hcom agents listening at clone time.
### Build caveat
If we later modify hcom's Rust source, rebuilding needs the Rust toolchain (`cargo`/`rustc`) — **not currently installed**. Doc/skill-only changes (the planned work) need no build.
### Related
(setup commit — see git log)

## 2026-05-22 — Codex setup docs and hosted DeepWiki doc source
### What
Added Codex project setup docs/index, installed hosted public DeepWiki MCP for Codex, and rewrote hcom README/docs/skill guidance using hosted public DeepWiki MCP source material plus local help/source verification.
### Why
The handoff requested DeepWiki-driven docs, and the user clarified that public GitHub repo work must use hosted DeepWiki MCP (`https://mcp.deepwiki.com/mcp`), not DeepWiki Local.
### How
Created project `AGENTS.md`, `CLAUDE.md`, `PROJECT_INDEX.json`, `docs/`, and `coordination/deepwiki-public-hcom-sourcepack.md`. Added Codex MCP server `deepwiki` in `~/.codex/config.toml` with `default_tools_approval_mode = "approve"`. Updated Codex-visible DeepWiki skill guidance to distinguish hosted public DeepWiki from `deepwiki-local`.
### Impact
Future Codex and Claude sessions in this repo have explicit DeepWiki MCP boundaries. Public repo doc work should cite `mcp__deepwiki__`; private/local filesystem analysis should cite `mcp__deepwiki_local__`.

## 2026-05-22 — Codex --yolo launch arg support
### What
Added hcom parser/preprocessing support for Codex's hidden `--yolo` launch flag.
### Why
`codex --yolo --version` accepts the flag locally, but `hcom codex --yolo` was rejected by hcom's Codex argument allowlist before launch.
### How
Added `--yolo` as a Codex boolean flag and sandbox-group override in `src/tools/codex_args.rs`; updated Codex preprocessing to treat it as sandbox-active for hcom writability handling; added regression tests and documentation notes.
### Impact
`hcom codex --yolo` should pass validation and override configured Codex sandbox mode the same way other user-provided sandbox/approval overrides do. Source-level tests still require installing Rust/Cargo in this environment.
