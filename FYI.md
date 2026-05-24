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

## 2026-05-22 — Rust toolchain installed and offline-verified
### What
Verified a Rust/Cargo setup suitable for building and testing hcom from source.
### Why
The Codex `--yolo` parser fix needed source-level Rust tests, and the user wanted the install audited with supply-chain risk in mind.
### How
Confirmed `rustup` was installed from Ubuntu `noble-updates/security` (`1.26.0-5ubuntu0.1`) and that distro `cargo`/`rustc` packages were not installed. Verified `/usr/bin/cargo` and `/usr/bin/rustc` are rustup shims, active stable toolchain is `rustc 1.95.0`, and `clippy`/`rustfmt` are present. Ran `cargo metadata --locked --offline --no-deps`, `cargo fmt --check`, `cargo test --locked --offline`, and `cargo clippy --all-targets --locked --offline -- -D warnings`.
### Impact
hcom can now be built and tested locally. Full offline locked tests and clippy pass; broader builds should continue to prefer `--locked` and `--offline` after dependencies are cached.

## 2026-05-23 — Merged upstream v0.7.18, swapped installed binary, opened upstream PR for `--yolo`
### What
Reconciled the fork against upstream's new v0.7.18 release, replaced the running uv-tool binary with a locally-built v0.7.18 that retains our fork-only `--yolo` support, and submitted the `--yolo` patch upstream as a clean single-commit PR.
### Why
The installed `hcom` (`uv tool install hcom`) was v0.7.17 from PyPI, lacking our fork-only `--yolo` patch (commit `b993078`) AND missing upstream's 5 v0.7.18 bug fixes. The user saw the v0.7.18 upgrade nag and wanted the fork to adopt v0.7.18 cleanly while keeping `--yolo`. Pre-merge audit (general-purpose subagent + codex:rescue cross-family security review) confirmed: zero new dependencies, zero CI workflow changes, zero install.sh changes, zero source-code red flags in the v0.7.18 delta; `git merge-tree` showed zero conflicts.
### How
1. Tagged safety point at fork HEAD `5977015` as `pre-merge-v0.7.18-20260523` (pushed to origin).
2. Created `feat/codex-yolo-flag` from `upstream/main`; checked out ONLY the 2 src files from `main` (`src/tools/codex_args.rs`, `src/tools/codex_preprocessing.rs`) to keep the upstream-PR commit free of fork-only docs (FYI.md, docs/*); committed as `9b764d5`; pushed to fork remote.
3. Opened upstream PR `aannoo/hcom#54` — 59 +/1 -, single commit, 4 new regression tests, no dependency changes.
4. Switched back to `main`, merged `upstream/main` (`git merge upstream/main`, ort strategy, zero conflicts, 18 files, +766/-177); merge commit `6d6e1ed`.
5. `cargo build --release --locked` (1m38s) + `cargo test --release --locked` (1550 pass / 0 fail, 9 ignored — PTY/relay/parser-drift gated on installed CLIs).
6. Replaced installed binary via mv-rename pattern (5 active hcom PTY processes held v0.7.17 binary as `txt` mapping — rename keeps old inode alive for them via open fd; cp puts v0.7.18 in place for new invocations). Preserved `~/.local/share/uv/tools/hcom/bin/hcom.0.7.17.bak` as rollback.
7. Smoke: `hcom --version` = `uvx hcom --version` = `0.7.18`; upgrade nag gone; `hcom codex --yolo --help` no longer rejects.
8. Pushed merged main to fork (`5977015..6d6e1ed`).
### Impact
- Installed CLI is now hcom 0.7.18 with fork's `--yolo` support live.
- `hcom codex --yolo` now works at the CLI without falling back to bare `codex --yolo`.
- Fork divergence from upstream is now exactly 4 commits (3 docs + the merge commit), all isolated to fork-only paths.
- Upstream PR #54 is the path to eliminate divergence: once aannoo accepts, future `uv tool upgrade hcom` will be safe again.
- **DO NOT** run `uv tool upgrade hcom` (or `uv tool install --reinstall hcom`) until PR #54 ships in upstream — that would re-pull PyPI v0.7.18 and silently lose `--yolo`. If accidentally run, repeat the local build+swap procedure.
- 5 still-running PTY agents (yumi/valo/lina/poke/peso) keep v0.7.17 via open fd; new launches get v0.7.18. Natural attrition is fine; force-restart only if you need v0.7.18 features in those specific sessions (codex --yolo, name reservation fix, Gemini parser drift fix).
- Cross-family security review caught a Codex hallucination ("v0.7.18 removes --yolo alias") — verified false via direct git inspection (zero `yolo` mentions in upstream commits). Standard verify-before-claiming applied.
### Related
- Fork merge commit `6d6e1ed`
- Fork PR-branch commit `9b764d5` (clean --yolo, src-only)
- Safety tag `pre-merge-v0.7.18-20260523`
- Upstream PR https://github.com/aannoo/hcom/pull/54
- Backup binary `~/.local/share/uv/tools/hcom/bin/hcom.0.7.17.bak`
