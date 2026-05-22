# HANDOFF — hcom (fork)

**Created by:** Claude Code (Opus 4.7 1M session, working from `/home/riche`)
**Date:** 2026-05-22
**Purpose:** Resume spec for the next Claude Code session that opens `~/MCPs/hcom/`. Written for a zero-context successor.

---

## WHAT THIS PROJECT IS

A fork (`RichelynScott/hcom`) of `aannoo/hcom` — a Rust CLI that hooks AI coding agents together for cross-terminal messaging. Cloned into `~/MCPs/` for local management. **Main task: refine hcom's README, docs, and skill** into a much more comprehensive form for our own use, using the **DeepWiki MCP**.

## IMMEDIATE NEXT ACTION

1. **Set up this directory per our protocols.** Fresh clone, not yet a configured Claude Code project:
   - `@setup-docs` / `/setup-docs` → project `CLAUDE.md`, `settings.json`, `.mcp.json` (`FYI.md` already exists).
   - `/index` → `PROJECT_INDEX.json`.
2. **MAIN TASK — DeepWiki-driven doc + skill refinement** (see methodology below).

## MAIN TASK — DeepWiki doc/skill refinement

Use the **DeepWiki MCP** (`mcp__claude_ai_DeepWiki_MCP__*`) against `aannoo/hcom` to produce a much better, more comprehensive set of docs for our fork:

- `mcp__claude_ai_DeepWiki_MCP__read_wiki_contents` — pull the full wiki body.
- `mcp__claude_ai_DeepWiki_MCP__ask_question` — drill into specifics (command semantics, config precedence, hook internals, relay).

Deliverables:
1. **Rewritten `README.md`** — comprehensive, accurate to v0.7.17, our-fork-aware.
2. **Expanded `docs/`** — per-area reference (architecture, command reference, config system, hook system, multi-agent comms, relay) — see the DeepWiki TOC seed below.
3. **Refined skill** — `skills/hcom-agent-messaging/SKILL.md` already exists; make it more comprehensive (the global `~/.claude/skills/hcom/` skill is separate — decide whether to sync).

### DeepWiki TOC seed (from `read_wiki_structure aannoo/hcom`, 2026-05-22)

Use this as the doc outline:
```
1 Overview · 2 Getting Started · 3 Core Concepts
4 Architecture (4.1 CLI Entry/Routing · 4.2 DB & Event Storage · 4.3 Instance Lifecycle ·
  4.4 Message Routing & Delivery · 4.5 Terminal Integration & PTY Wrapper)
5 Command Reference (5.1 Launch/Instance · 5.2 Messaging · 5.3 Events/Query ·
  5.4 Config/Management · 5.5 Terminal)
6 Configuration System (6.1 Files & Precedence · 6.2 Terminal Presets · 6.3 Settings Reference)
7 Tool Integration (7.1 Hook System · 7.2 Identity/Session Binding · 7.3 Claude Code · 7.4 PTY Testing)
8 Multi-Agent Communication (8.1 Scopes & Delivery · 8.2 Event Subscriptions ·
  8.3 Bundles & Context Sharing · 8.4 Instance Forking & Subagents)
9 Cross-Device Sync (9.1 Relay Architecture · 9.2 Relay Setup)
10 Build & Distribution · 11 Database Schema Reference
12 Development Guide · 13 Agent Skills & Workflow Patterns · 14 Glossary
```

## CURRENT STATE

| Item | State |
|---|---|
| Repo cloned (`~/MCPs/hcom`) | ✅ v0.7.17, == upstream, == installed binary |
| Reinstall | ✅ NOT needed — installed `hcom` already current (verified) |
| Directory setup (CLAUDE.md / .mcp.json / PROJECT_INDEX) | ❌ NOT done — step 1 |
| DeepWiki doc/skill refinement | ❌ NOT started — MAIN TASK |

## CRITICAL RULES (put in project CLAUDE.md)

1. **Doc/skill changes need no build.** If you change Rust source (`src/`), rebuilding needs `cargo`/`rustc` — not installed. Install the Rust toolchain first if so.
2. The installed `hcom` (`~/.local/bin/hcom`, `uv tool`) is the live binary. Doc changes in this repo do NOT affect the running binary.
3. This is a fork — keep it rebaseable on `aannoo/hcom`. Put our customizations in clearly-scoped paths (`docs/`, README sections) so upstream merges stay clean.
4. hcom config lives at `~/.hcom/config.toml`; DB at `~/.hcom/hcom.db`.

## RELATED WORKSTREAM

Sibling projects set up the same session: `~/MCPs/claude-context-bridge/` and `~/MCPs/codex-plugin-cc/` — each has its own `HANDOFF.md`. All three are part of the same `mcp-adopt` hcom group (see below).

## HCOM GROUP

This session should be launched as part of the `mcp-adopt` hcom tag group. The user spawns it via `HCOM_TAG=mcp-adopt hcom claude` from this directory. Address the group with `@mcp-adopt`; address peers individually with `@mcp-adopt-<name>`.

## SESSION-SUCCESSION NOTE

Plain handoff (this file). The prior `/home/riche` session is not a project-role owner; `@session-succession` overseer pattern is available but not required.
