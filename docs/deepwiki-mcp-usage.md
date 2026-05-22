# DeepWiki MCP Usage

This project distinguishes hosted public DeepWiki MCP from DeepWiki Local. They are related tools, but they are not interchangeable.

## Hosted Public DeepWiki MCP

Use hosted DeepWiki for public GitHub repositories and generated wiki reads.

- Server: `https://mcp.deepwiki.com/mcp`
- Codex config name: `deepwiki`
- Codex tool namespace in fresh sessions: `mcp__deepwiki__`
- Claude entry: commonly shown as `DeepWiki_MCP`
- Repository argument shape: `owner/repo`, for example `aannoo/hcom`

Codex setup:

```bash
codex mcp add deepwiki --url https://mcp.deepwiki.com/mcp
codex mcp get deepwiki
```

Expected Codex config:

```toml
[mcp_servers.deepwiki]
url = "https://mcp.deepwiki.com/mcp"
default_tools_approval_mode = "approve"
```

Common public tool calls:

- `mcp__deepwiki__.read_wiki_structure`
- `mcp__deepwiki__.read_wiki_contents`
- `mcp__deepwiki__.ask_question`

Use this path for README/docs/skill work about upstream public repos unless the user explicitly asks for the self-hosted local server.

## DeepWiki Local

Use DeepWiki Local for private repositories, local filesystem paths, offline/container-backed RAG, and local cache/cost inspection.

- Codex config name: `deepwiki-local`
- Codex tool namespace: `mcp__deepwiki_local__`
- Runtime repository: `/home/riche/MCPs/deepwiki-open`
- Local service: `http://localhost:8001`
- Mounted path scope: `/home/riche/Proj/*` and `/home/riche/MCPs/*`

Do not use DeepWiki Local for public GitHub repo DeepWiki work just because the task says "DeepWiki MCP". If the repo is public and the user references DeepWiki MCP, prefer hosted public `deepwiki`.

## Evidence Pattern

When a session uses DeepWiki, record the source explicitly:

```text
Used public DeepWiki MCP: mcp__deepwiki__.read_wiki_contents(repoName="aannoo/hcom").
Did not use mcp__deepwiki_local__.
```

or:

```text
Used DeepWiki Local: mcp__deepwiki_local__.analyze_local_repo(path="/home/riche/MCPs/hcom").
Reason: local/private filesystem analysis was requested.
```
