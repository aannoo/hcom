"use strict";
var __create = Object.create;
var __defProp = Object.defineProperty;
var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
var __getOwnPropNames = Object.getOwnPropertyNames;
var __getProtoOf = Object.getPrototypeOf;
var __hasOwnProp = Object.prototype.hasOwnProperty;
var __export = (target, all) => {
  for (var name in all)
    __defProp(target, name, { get: all[name], enumerable: true });
};
var __copyProps = (to, from, except, desc) => {
  if (from && typeof from === "object" || typeof from === "function") {
    for (let key of __getOwnPropNames(from))
      if (!__hasOwnProp.call(to, key) && key !== except)
        __defProp(to, key, { get: () => from[key], enumerable: !(desc = __getOwnPropDesc(from, key)) || desc.enumerable });
  }
  return to;
};
var __toESM = (mod, isNodeMode, target) => (target = mod != null ? __create(__getProtoOf(mod)) : {}, __copyProps(
  // If the importer is in node compatibility mode or this is not an ESM
  // file that has been converted to a CommonJS file using a Babel-
  // compatible transform (i.e. "__esModule" has not been set), then set
  // "default" to the CommonJS "module.exports" for node compatibility.
  isNodeMode || !mod || !mod.__esModule ? __defProp(target, "default", { value: mod, enumerable: true }) : target,
  mod
));
var __toCommonJS = (mod) => __copyProps(__defProp({}, "__esModule", { value: true }), mod);

// src/extension.ts
var extension_exports = {};
__export(extension_exports, {
  activate: () => activate,
  deactivate: () => deactivate
});
module.exports = __toCommonJS(extension_exports);

// src/hcomClient.ts
var cp = __toESM(require("child_process"));
var vscode = __toESM(require("vscode"));
var HcomClient = class {
  constructor() {
    this.process = null;
    this.onMessageHandlers = [];
    this.onStatusHandlers = [];
    this.restartTimeout = null;
    this._disposed = false;
  }
  get isRunning() {
    return this.process !== null && !this.process.killed;
  }
  onMessage(handler) {
    this.onMessageHandlers.push(handler);
    return { dispose: () => this.offMessage(handler) };
  }
  onStatus(handler) {
    this.onStatusHandlers.push(handler);
    return { dispose: () => this.offStatus(handler) };
  }
  offMessage(handler) {
    this.onMessageHandlers = this.onMessageHandlers.filter((h) => h !== handler);
  }
  offStatus(handler) {
    this.onStatusHandlers = this.onStatusHandlers.filter((h) => h !== handler);
  }
  emitMessage(sender, text) {
    for (const handler of this.onMessageHandlers) {
      try {
        handler(sender, text);
      } catch {
      }
    }
  }
  emitStatus(status) {
    for (const handler of this.onStatusHandlers) {
      try {
        handler(status);
      } catch {
      }
    }
  }
  start() {
    if (this._disposed) return;
    this.spawn();
  }
  spawn() {
    if (this._disposed) return;
    this.kill();
    const config = vscode.workspace.getConfiguration("hcom.bridge");
    const binaryPath = config.get("binaryPath", "hcom");
    const project = config.get("project", "");
    const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
    const args = ["listen"];
    if (project) {
      args.push("--project", project);
    }
    if (workspaceRoot) {
      args.push("--cwd", workspaceRoot);
    }
    const options = {
      stdio: ["ignore", "pipe", "pipe"],
      detached: false
    };
    if (workspaceRoot) {
      options.cwd = workspaceRoot;
    }
    this.emitStatus("connecting");
    const proc = cp.spawn(binaryPath, args, options);
    this.process = proc;
    let buffer = "";
    proc.stdout?.on("data", (data) => {
      buffer += data.toString();
      this.parseBuffer(buffer, (msg) => {
        buffer = buffer.slice(buffer.indexOf(msg) + msg.length + 1);
        const parsed = this.parseHcomMessage(msg);
        if (parsed) {
          this.emitMessage(parsed.sender, parsed.text);
        }
      });
    });
    proc.stderr?.on("data", (data) => {
      const text = data.toString().trim();
      if (text) {
        this.emitStatus(`stderr: ${text}`);
      }
    });
    proc.on("error", (err) => {
      this.emitStatus(`error: ${err.message}`);
      this.scheduleRestart();
    });
    proc.on("exit", (code) => {
      this.process = null;
      this.emitStatus(`exited (${code})`);
      if (!this._disposed) {
        this.scheduleRestart();
      }
    });
    this.emitStatus("listening");
  }
  parseBuffer(buffer, callback) {
    const lines = buffer.split("\n");
    for (const line of lines) {
      if (line.includes("[hcom]") || line.includes("<hcom>")) {
        callback(line);
      }
    }
  }
  parseHcomMessage(line) {
    const match = line.match(/@(\S+)\s+(.+)/);
    if (match) {
      return { sender: match[1], text: match[2] };
    }
    const hcomMatch = line.match(/<hcom>.*?(\S+)\s*→\s*\S+\s*:\s*(.+)/);
    if (hcomMatch) {
      return { sender: hcomMatch[1], text: hcomMatch[2] };
    }
    return null;
  }
  async send(target, message) {
    const config = vscode.workspace.getConfiguration("hcom.bridge");
    const binaryPath = config.get("binaryPath", "hcom");
    const project = config.get("project", "");
    const args = ["send", `@${target}`, "--", message];
    if (project) {
      args.push("--project", project);
    }
    return new Promise((resolve, reject) => {
      cp.execFile(binaryPath, args, { timeout: 1e4 }, (err, stdout, stderr) => {
        if (err) reject(new Error(stderr || err.message));
        else resolve(stdout.trim());
      });
    });
  }
  async listAgents() {
    const config = vscode.workspace.getConfiguration("hcom.bridge");
    const binaryPath = config.get("binaryPath", "hcom");
    const project = config.get("project", "");
    const args = ["list", "--names"];
    if (project) {
      args.push("--project", project);
    }
    return new Promise((resolve, reject) => {
      cp.execFile(binaryPath, args, { timeout: 5e3 }, (err, stdout) => {
        if (err) reject(err);
        else resolve(stdout.trim());
      });
    });
  }
  scheduleRestart() {
    if (this._disposed) return;
    if (this.restartTimeout) {
      clearTimeout(this.restartTimeout);
    }
    this.restartTimeout = setTimeout(() => this.spawn(), 3e3);
    this.emitStatus("reconnecting in 3s...");
  }
  kill() {
    if (this.restartTimeout) {
      clearTimeout(this.restartTimeout);
      this.restartTimeout = null;
    }
    if (this.process && !this.process.killed) {
      try {
        this.process.kill();
      } catch {
      }
    }
    this.process = null;
  }
  dispose() {
    this._disposed = true;
    this.kill();
    this.onMessageHandlers = [];
    this.onStatusHandlers = [];
  }
};

// src/jetskiBridge.ts
var vscode2 = __toESM(require("vscode"));
var JetskiBridge = class {
  constructor(hcomClient2) {
    this.disposables = [];
    this.hcomClient = hcomClient2;
    this.statusBarItem = vscode2.window.createStatusBarItem(
      vscode2.StatusBarAlignment.Left,
      100
    );
    this.statusBarItem.command = "hcom.showStatus";
    this.statusBarItem.tooltip = "hcom Bridge \u2014 click for status";
    this.statusBarItem.show();
    this.updateStatusBar("initializing");
  }
  activate(context) {
    this.registerListeners();
    this.registerCommands(context);
    this.registerChatParticipant(context);
    this.updateStatusBar("active");
  }
  registerListeners() {
    this.disposables.push(
      this.hcomClient.onMessage((sender, text) => {
        this.forwardToJetski(sender, text);
      })
    );
    this.disposables.push(
      this.hcomClient.onStatus((status) => {
        this.updateStatusBar(status);
      })
    );
  }
  registerCommands(context) {
    context.subscriptions.push(
      vscode2.commands.registerCommand("hcom.listAgents", async () => {
        try {
          const agents = await this.hcomClient.listAgents();
          vscode2.window.showInformationMessage(`hcom agents:
${agents}`);
        } catch (err) {
          const msg = err instanceof Error ? err.message : String(err);
          vscode2.window.showErrorMessage(`Failed to list agents: ${msg}`);
        }
      })
    );
    context.subscriptions.push(
      vscode2.commands.registerCommand("hcom.sendMessage", async () => {
        const target = await vscode2.window.showInputBox({
          prompt: "Target agent name",
          placeHolder: "e.g. moto, sora, yuri"
        });
        if (!target) return;
        const message = await vscode2.window.showInputBox({
          prompt: `Message to @${target}`,
          placeHolder: "Enter your message"
        });
        if (!message) return;
        try {
          const result = await this.hcomClient.send(target, message);
          vscode2.window.showInformationMessage(`Sent: ${result}`);
        } catch (err) {
          const msg = err instanceof Error ? err.message : String(err);
          vscode2.window.showErrorMessage(`Send failed: ${msg}`);
        }
      })
    );
    context.subscriptions.push(
      vscode2.commands.registerCommand("hcom.showStatus", () => {
        const running = this.hcomClient.isRunning;
        const status = running ? "Connected" : "Disconnected";
        vscode2.window.showInformationMessage(
          `hcom Bridge: ${status}

Binary: ${vscode2.workspace.getConfiguration("hcom.bridge").get("binaryPath", "hcom")}
Project: ${vscode2.workspace.getConfiguration("hcom.bridge").get("project", "(none)")}`
        );
      })
    );
  }
  registerChatParticipant(context) {
    const participant = vscode2.chat.createChatParticipant("hcom.hcom", async (request, _context, response, _token) => {
      const prompt = request.prompt.trim();
      if (prompt.startsWith("list") || prompt === "agents" || prompt === "ls") {
        try {
          const agents = await this.hcomClient.listAgents();
          response.markdown(`**hcom active agents:**

\`\`\`
${agents}
\`\`\``);
        } catch (err) {
          const msg = err instanceof Error ? err.message : String(err);
          response.markdown(`Error: ${msg}`);
        }
        return;
      }
      const sendMatch = prompt.match(/^(?:send\s+)?@?(\S+)\s+(.+)/);
      if (sendMatch) {
        const target = sendMatch[1];
        const message = sendMatch[2];
        try {
          await this.hcomClient.send(target, message);
          response.markdown(`\u2705 Sent message to @${target}`);
        } catch (err) {
          const msg = err instanceof Error ? err.message : String(err);
          response.markdown(`\u274C Failed: ${msg}`);
        }
        return;
      }
      response.markdown(
        `Usage:

- \`@hcom list\` \u2014 list active agents
- \`@hcom send @name message\` \u2014 send message to agent
- \`@hcom @name message\` \u2014 shorthand (auto-detect as send)
- \`@hcom status\` \u2014 show bridge status
- \`@hcom help\` \u2014 this help`
      );
    });
    participant.followupProvider = {
      provideFollowups(_result, _token) {
        return [
          { prompt: "list", label: "List agents" },
          { prompt: "status", label: "Bridge status" }
        ];
      }
    };
    context.subscriptions.push(participant);
  }
  forwardToJetski(sender, text) {
    const config = vscode2.workspace.getConfiguration("hcom.bridge");
    const autoSend = config.get("autoSendToJetski", true);
    if (!autoSend) return;
    const ext = this.getAntigravityExt();
    if (!ext) {
      this.updateStatusBar("no antigravity API");
      return;
    }
    try {
      const message = `**[hcom] @${sender}**

${text}`;
      ext.sendToAgentPanel({ message, autoSend: true });
    } catch {
      this.updateStatusBar("send failed");
    }
  }
  getAntigravityExt() {
    try {
      const win = vscode2.window;
      if (win.antigravityExtensibility && typeof win.antigravityExtensibility.sendToAgentPanel === "function") {
        return win.antigravityExtensibility;
      }
    } catch {
    }
    return void 0;
  }
  updateStatusBar(status) {
    const icons = {
      "active": "$(broadcast)",
      "listening": "$(broadcast)",
      "connected": "$(broadcast)",
      "connecting": "$(sync~spin)",
      "reconnecting": "$(sync~spin)",
      "error": "$(error)",
      "exited": "$(debug-disconnect)",
      "initializing": "$(loading~spin)"
    };
    const icon = Object.entries(icons).find(([k]) => status.startsWith(k))?.[1] || "$(question)";
    this.statusBarItem.text = `${icon} hcom`;
    this.statusBarItem.backgroundColor = status.includes("error") || status.startsWith("exited") ? new vscode2.ThemeColor("statusBarItem.errorBackground") : void 0;
  }
  dispose() {
    this.statusBarItem.dispose();
    for (const d of this.disposables) {
      d.dispose();
    }
    this.disposables = [];
  }
};

// src/extension.ts
var hcomClient;
var jetskiBridge;
function activate(context) {
  hcomClient = new HcomClient();
  jetskiBridge = new JetskiBridge(hcomClient);
  context.subscriptions.push(hcomClient);
  context.subscriptions.push(jetskiBridge);
  hcomClient.start();
  jetskiBridge.activate(context);
  console.log("hcom Bridge activated");
}
function deactivate() {
  if (hcomClient) {
    hcomClient.dispose();
    hcomClient = void 0;
  }
  if (jetskiBridge) {
    jetskiBridge.dispose();
    jetskiBridge = void 0;
  }
  console.log("hcom Bridge deactivated");
}
// Annotate the CommonJS export names for ESM import in node:
0 && (module.exports = {
  activate,
  deactivate
});
