# Antigravity + Gemini CLI — hcom Integration Analysis

## Overview

Two Google products were investigated for hcom integration, plus a third community
project that shares the name. Their integration potential differs significantly.

---

## 1. Antigravity (Google's VS Code fork)

**Identity:** Google-branded VS Code fork, Electron IDE (v1.107.0)
**Binary:** `antigravity` (`/usr/bin/antigravity → /opt/Antigravity/bin/antigravity`)
**Runtime:** Electron (Node.js v22.20 + Chromium)
**Package:** `apt antigravity`
**Config:** `~/.gemini/antigravity/` (mcp_config.json, conversations/, brain/)
**Alias:** `agy`
**Data folder:** `~/.antigravity/`

### CLI interface

```bash
antigravity chat [options] [prompt]   # Open AI chat in GUI (modes: ask|edit|agent)
antigravity chat -m agent <prompt> -  # Pipe stdin, but STILL opens GUI window
antigravity serve-web                 # Web UI server (no AI chat there)
antigravity tunnel                    # Secure tunnel to vscode.dev
```

### `antigravity chat` flags

| Flag | Description |
|------|-------------|
| `-m --mode <ask\|edit\|agent>` | Chat mode (default: agent) |
| `-a --add-file <path>` | Add files as context |
| `--maximize` | Maximize chat view |
| `-r --reuse-window` | Reuse existing window |
| `-n --new-window` | Open new empty window |
| `--profile <name>` | Use specific profile |

**Pipe mode** exists (`antigravity chat prompt -`) but **still opens a GUI window**
— it reads stdin into a temp file and shows it in the chat panel.

### Probed capabilities

| Capability | Result |
|------------|--------|
| Headless AI (no GUI) | ❌ `chat` always opens Electron window |
| Pipe mode (`-p`/stdin) | ⚠️ Reads stdin but still shows GUI |
| JSON output | ❌ No `--json` or `--output-format` |
| Hook scripts (`--hooks-dir`) | ❌ Not available |
| ACP protocol (`--acp`) | ❌ Not available |
| Auto-approve (`--yolo`) | ❌ Not available |
| Programmatic Agent API | ❌ No equivalent to ClineAgent |
| MCP server registration | ✅ `--add-mcp <json>` + `mcp_config.json` |
| VS Code extensions | ✅ Standard VS Code extension system |
| Transient mode | ✅ `--transient` (temp data/ext dirs) |
| Web server mode | ✅ `serve-web` but no AI chat there |
| Wait-for-completion | ❌ `--wait` not supported by `chat` subcommand |
| Listen/notify | ❌ None |

### Viable integration approaches (ranked by effort)

#### A. MCP-based (lowest effort, limited)

Register hcom as an MCP server in `~/.gemini/antigravity/mcp_config.json`:

```json
{
  "hcom": {
    "command": "hcom",
    "args": ["mcp", "--serve"],
    "env": { "HCOM_PROJECT": "..." }
  }
}
```

This gives Antigravity access to hcom **tools** (send, list, events, etc.) via
MCP tool calls. Good for: sending messages to hcom agents from within Antigravity.
Does NOT enable: spawning Antigravity as an hcom agent, lifecycle management,
bidirectional message delivery.

#### B. VS Code extension (medium effort, bidirectional)

Write a VS Code extension for Antigravity that bridges to hcom:
- Listens for Antigravity session events
- Forwards messages between hcom and Antigravity chat
- Registers commands like "Send to hcom"

The Antigravity extension (`extensions/antigravity/`) powers all AI features
and uses `enabledApiProposals` including `antigravityUnifiedStateSync` — there
IS a proposed API for state sync that could be leveraged.

#### C. serve-web + automation (high effort, fragile)

`antigravity serve-web` starts a web server. Could be automated with browser
automation (Playwright/Puppeteer) to interact with the AI chat. Very fragile.

#### D. PTY spawning (not feasible)

`antigravity chat --mode agent --new-window` opens a GUI. Even with
`--new-window --transient`, it spawns a full Electron process. The PTY wrapper
would just see Electron output, not the AI agent's responses.

### New finding: antigravity-cli (file-based async tasks)

Repo: github.com/michaelw9999/antigravity-cli (13★, Python)

This unofficial CLI reads/writes Antigravity's task artifacts in
`~/.gemini/antigravity/brain/<GUID>/`. It allows **file-based asynchronous**
task delegation:

```bash
# Create a task → Jetski (Antigravity's AI) picks it up
antigravity-cli new-task "Implement feature X"

# Read task progress
antigravity-cli show-task <GUID>

# Inject context back
antigravity-cli write-artifact --artifacttype implementation_plan \
  --primarytask <GUID> --filepath ./context.md --summary "hcom message"

# Update task status
antigravity-cli update-task --primarytask <GUID> --subtask 0 --state completed
```

This is **async file-based** communication, not real-time bidirectional
messaging. But it DOES enable hcom to delegate work to Antigravity's AI agent
(Jetski) by creating tasks in its brain directory.

## 4. VS Code Extension for hcom ↔ Antigravity Bridge

**Veredicto: SÍ es posible.** Una extensión VS Code puede integrar hcom con
Antigravity usando APIs estándar y del propio Antigravity.

### APIs disponibles para la extensión

| API | Capacidad | Documentada? |
|-----|-----------|-------------|
| `antigravityExtensibility.sendToAgentPanel({message, files, autoSend})` | Enviar mensajes al agente Jetski | ✅ Pública y estable |
| `vscode.lm.selectChatModels()` + `sendRequest()` | Enviar prompts al LM y recibir streaming responses | ✅ VS Code API estándar |
| `vscode.chat.createChatParticipant("hcom", handler)` | Crear participante `@hcom` en el chat | ✅ VS Code API estándar |
| `vscode.lm.registerLanguageModelChatProvider("hcom", provider)` | Registrar hcom como proveedor de LM | ✅ VS Code API estándar |
| `vscode.lm.registerTool()` | Registrar tools para tool-calling | ✅ VS Code API estándar |
| `child_process.spawn()` | Spawnear hcom CLI como child process | ✅ Extension host |
| `vscode.commands.executeCommand("antigravity.*")` | Comandos internos (readTerminal, etc.) | ⚠️ No documentados |

### Arquitectura propuesta

```
┌─────────────────────────────────────────────────┐
│ Antigravity (Electron)                          │
│                                                  │
│  ┌─────────────────────────────────────────┐    │
│  │ Extension: hcom-bridge                   │    │
│  │                                          │    │
│  │  spawns ──► hcom listen (child proc)     │    │
│  │      │                                    │    │
│  │      ├► sendToAgentPanel() → Jetski      │    │
│  │      │   (mensajes entrantes de hcom)     │    │
│  │      │                                    │    │
│  │      ├► ChatParticipant("@hcom")          │    │
│  │      │   (mensajes salientes a hcom)      │    │
│  │      │                                    │    │
│  │      └► LM.sendRequest() → lee respuestas │    │
│  │         (streaming de Jetski a hcom)       │    │
│  └─────────────────────────────────────────┘    │
│                                                  │
│  ┌─────────────────────────────────────────┐    │
│  │ Jetski Agent (language_server_linux_x64) │    │
│  │ Loop conversando con el LLM, tools, etc  │    │
│  └─────────────────────────────────────────┘    │
└─────────────────────────────────────────────────┘
         ▲
         │ hcom send @name ...
         ▼
┌─────────────────────────────────────────────────┐
│ Otros agentes hcom (moto, sora, yuri, etc)       │
└─────────────────────────────────────────────────┘
```

### Flujo bidireccional

**hcom → Antigravity (mensaje entrante):**
1. La extensión corre `hcom listen` como child process
2. Detecta nuevo mensaje via stdout/notify
3. Llama `sendToAgentPanel({message: "...", autoSend: true})`
4. Jetski recibe el mensaje en el panel de chat y lo procesa

**Antigravity → hcom (mensaje saliente):**
1. Usuario escribe `@hcom enviar mensaje...` en el chat de Antigravity
2. `ChatParticipant("hcom")` recibe el `ChatRequest`
3. La extensión ejecuta `hcom send @agent -- "mensaje"`
4. El mensaje se entrega al agente hcom destino

### Consideraciones

- **No se puede spawnear Antigravity como agente** — Antigravity es un IDE
  Electron, no un CLI headless. La extensión corre DENTRO de Antigravity.
- **No hay API para recibir responses de `sendToAgentPanel`** — es fire-and-forget.
  Para recibir respuestas, usar `vscode.lm.sendRequest()` al LM subyacente.
- **El agente Jetski es un binary externo** (`language_server_linux_x64`) que la
  extensión principal de Antigravity gestiona. No hay API pública para
  controlarlo directamente.
- **La extensión se publica en open-vsx.org** (el marketplace de Antigravity)
  y se instala con `antigravity --install-extension hcom-bridge`.

### Veredicto final

| Approach | Tiempo real? | Bidireccional? | Esfuerzo |
|----------|-------------|---------------|----------|
| MCP server | ✅ sync | ⚠️ Solo tools (Antigravity→hcom) | Bajo |
| antigravity-cli (file tasks) | ❌ async | ✅ Ambos sentidos | Bajo |
| **VS Code extension** | ✅ **sync** | **✅ Completo** | **Medio** |

La extensión VS Code es la **única forma de lograr integración completa en
tiempo real** entre hcom y Antigravity. Las otras opciones (MCP, antigravity-cli)
son parciales o asíncronas.

---

## 2. Gemini CLI (`gemini`) — Google's actual AI coding CLI

**Identity:** Google's official AI coding CLI (like Claude Code)
**Binary:** `gemini`
**Install:** `npm install -g @google/gemini-cli` | `brew install gemini-cli`
**Runtime:** Node.js (TypeScript, open source)
**GitHub:** github.com/google-gemini/gemini-cli — 103k stars
**Config:** `~/.gemini/settings.json`
**License:** Apache 2.0
**Free tier:** 60 req/min, 1000 req/day

### CLI flags

| Flag | Description |
|------|-------------|
| `gemini -p "prompt"` | Non-interactive mode (like Claude -p) |
| `--output-format json` | JSON structured output |
| `--output-format stream-json` | Streaming JSON output |
| `--include-directories` | Multi-directory context |
| `-m <model>` | Model selection |
| `gemini task "prompt"` | Task mode |

### Integration assessment

| Capability | Status |
|------------|--------|
| Headless/pipe mode | ✅ `gemini -p` |
| JSON output | ✅ `--output-format json` |
| MCP support | ✅ (in settings.json) |
| Hook system | ⚠️ Needs repo clone to confirm |
| Programmatic API | ⚠️ Needs repo clone |
| Auto-approve | ❓ Unknown (likely --yes flag) |

### Viable approach: CLI spawning + MCP

```bash
hcom 1 gemini -- "Write a parser for X"
```

Two sub-approaches:
1. **PTY spawning** (like OpenCode/Kilo/Cline) — spawn in terminal, inject via PTY
2. **Pipe mode** (like `claude -p`) — use `gemini -p` with `--output-format json`

Needs further research: hook system, lifecycle events, extension API.

---

## 3. antigravity-workspace-template (community project)

**Author:** study8677
**Repos:** github.com/study8677/antigravity-workspace-template (1.2k★)
**Language:** Python
**Description:** Multi-agent knowledge engine. Has own CLI (`ag init`, `ag refresh`,
`ag ask`, `ag-mcp`). Runs as MCP server. Compatible with Claude Code, Codex,
Cursor, Windsurf, Gemini CLI.

**Not relevant for hcom integration** — this is a separate community project,
not Google's Antigravity.

---

## Feasibility Summary

| Tool | Spawn as agent? | Bidirectional msgs? | Lifecycle mgmt? | Effort |
|------|----------------|-------------------|-----------------|--------|
| **Antigravity (IDE)** | ❌ No headless AI | ⚠️ MCP (tools→hcom) / ✅ antigravity-cli (async) | ❌ | Low (MCP+cli) |
| **Gemini CLI** | ✅ Already integrated | ✅ | ✅ | Already done |
| **antigravity-workspace-template** | ❌ MCP server only | — | — | Low (MCP add) |

## Recommended approach

**Antigravity como MCP client** (ya funcional):

```bash
hcom mcp --serve --project X
# Then register in Antigravity:
antigravity --add-mcp '{"name":"hcom","command":"hcom","args":["mcp","--serve","--project","X"]}'
```

Esto permite que el agente de Antigravity (Jetski) llame tools de hcom:
`send`, `list`, `events`, `transcript`, etc. — directamente desde el chat.

**Para "tener antigravity como agente trabajador"** (multiples instancias):
No es posible sin una extensión VS Code custom que bridgee hcom ↔ Antigravity.
El `chat --mode agent` siempre abre una ventana Electron — no hay modo headless.

## Auto-install: `hcom hooks add antigravity`

La extensión está integrada en el sistema de hooks de hcom:

```bash
# Instalar la extensión en Antigravity
hcom hooks add antigravity

# Verificar estado
hcom hooks status | grep antigravity

# Remover
hcom hooks remove antigravity
```

Esto:
1. Escribe `extension.js` + `package.json` en `~/.antigravity/extensions/hcom.hcom-bridge-0.1.0/`
2. Actualiza `~/.antigravity/extensions/extensions.json`
3. La extensión se activa al reiniciar Antigravity

La extensión compilada (~13KB) está embedida en el binario de hcom via
`include_str!` — no requiere npm/TypeScript en la máquina del usuario.

### Source

- Extensión TS: `extensions/hcom-bridge/`
- Build: `extensions/hcom-bridge/build.sh`
- Embed Rust: `src/hooks/antigravity.rs`
- Hook wiring: `src/commands/hooks.rs`

---

*Research by: @sora, @moto, @yuri — May 2026*
