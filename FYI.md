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
