# hcom Fork Claude Notes

This file is a project-local Claude Code orientation note. Codex-owned instructions live in `AGENTS.md`; keep both files aligned when changing project operating rules.

## Project Purpose

This repository is `RichelynScott/hcom`, a fork of `aannoo/hcom`. The current local goal is documentation and skill refinement for hcom while keeping the fork easy to rebase onto upstream.

## DeepWiki MCP Boundary

Use hosted public DeepWiki MCP for public GitHub repositories:

- Server: `https://mcp.deepwiki.com/mcp`
- Claude list entry should appear as `DeepWiki_MCP`
- Codex namespace when exposed: `mcp__deepwiki__`
- Public repo argument form: `owner/repo`, for example `aannoo/hcom`

Use DeepWiki Local only for local/private/offline analysis:

- Codex namespace: `mcp__deepwiki_local__`
- Runtime container: `/home/riche/MCPs/deepwiki-open`, `localhost:8001`
- Scope: private repos, `/home/riche/Proj/*`, `/home/riche/MCPs/*`, local cache/cost work

Do not use DeepWiki Local as a substitute for hosted DeepWiki MCP when a task references public GitHub repos or Devin's DeepWiki MCP documentation.

## Runtime Boundaries

- hcom runtime state: `~/.hcom`.
- Codex runtime config: `~/.codex`.
- Claude runtime config: `~/.claude`.
- Do not commit relay tokens, PSKs, local DBs, hook logs, or transient runtime state.

## Verification

For docs or skill work based on DeepWiki, record which MCP namespace was used. Good evidence:

```text
Used public DeepWiki MCP: mcp__deepwiki__.read_wiki_structure(repoName="aannoo/hcom").
Did not use mcp__deepwiki_local__.
```
