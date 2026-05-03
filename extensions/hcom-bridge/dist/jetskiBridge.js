"use strict";
var __createBinding = (this && this.__createBinding) || (Object.create ? (function(o, m, k, k2) {
    if (k2 === undefined) k2 = k;
    var desc = Object.getOwnPropertyDescriptor(m, k);
    if (!desc || ("get" in desc ? !m.__esModule : desc.writable || desc.configurable)) {
      desc = { enumerable: true, get: function() { return m[k]; } };
    }
    Object.defineProperty(o, k2, desc);
}) : (function(o, m, k, k2) {
    if (k2 === undefined) k2 = k;
    o[k2] = m[k];
}));
var __setModuleDefault = (this && this.__setModuleDefault) || (Object.create ? (function(o, v) {
    Object.defineProperty(o, "default", { enumerable: true, value: v });
}) : function(o, v) {
    o["default"] = v;
});
var __importStar = (this && this.__importStar) || (function () {
    var ownKeys = function(o) {
        ownKeys = Object.getOwnPropertyNames || function (o) {
            var ar = [];
            for (var k in o) if (Object.prototype.hasOwnProperty.call(o, k)) ar[ar.length] = k;
            return ar;
        };
        return ownKeys(o);
    };
    return function (mod) {
        if (mod && mod.__esModule) return mod;
        var result = {};
        if (mod != null) for (var k = ownKeys(mod), i = 0; i < k.length; i++) if (k[i] !== "default") __createBinding(result, mod, k[i]);
        __setModuleDefault(result, mod);
        return result;
    };
})();
Object.defineProperty(exports, "__esModule", { value: true });
exports.JetskiBridge = void 0;
const vscode = __importStar(require("vscode"));
/// <reference path="./antigravity.d.ts" />
class JetskiBridge {
    constructor(hcomClient) {
        this.disposables = [];
        this.hcomClient = hcomClient;
        this.statusBarItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 100);
        this.statusBarItem.command = 'hcom.showStatus';
        this.statusBarItem.tooltip = 'hcom Bridge — click for status';
        this.statusBarItem.show();
        this.updateStatusBar('initializing');
    }
    activate(context) {
        this.registerListeners();
        this.registerCommands(context);
        this.registerChatParticipant(context);
        this.updateStatusBar('active');
    }
    registerListeners() {
        this.disposables.push(this.hcomClient.onMessage((sender, text) => {
            this.forwardToJetski(sender, text);
        }));
        this.disposables.push(this.hcomClient.onStatus((status) => {
            this.updateStatusBar(status);
        }));
    }
    registerCommands(context) {
        context.subscriptions.push(vscode.commands.registerCommand('hcom.listAgents', async () => {
            try {
                const agents = await this.hcomClient.listAgents();
                vscode.window.showInformationMessage(`hcom agents:\n${agents}`);
            }
            catch (err) {
                const msg = err instanceof Error ? err.message : String(err);
                vscode.window.showErrorMessage(`Failed to list agents: ${msg}`);
            }
        }));
        context.subscriptions.push(vscode.commands.registerCommand('hcom.sendMessage', async () => {
            const target = await vscode.window.showInputBox({
                prompt: 'Target agent name',
                placeHolder: 'e.g. moto, sora, yuri'
            });
            if (!target)
                return;
            const message = await vscode.window.showInputBox({
                prompt: `Message to @${target}`,
                placeHolder: 'Enter your message'
            });
            if (!message)
                return;
            try {
                const result = await this.hcomClient.send(target, message);
                vscode.window.showInformationMessage(`Sent: ${result}`);
            }
            catch (err) {
                const msg = err instanceof Error ? err.message : String(err);
                vscode.window.showErrorMessage(`Send failed: ${msg}`);
            }
        }));
        context.subscriptions.push(vscode.commands.registerCommand('hcom.showStatus', () => {
            const running = this.hcomClient.isRunning;
            const status = running ? 'Connected' : 'Disconnected';
            vscode.window.showInformationMessage(`hcom Bridge: ${status}\n\n` +
                `Binary: ${vscode.workspace.getConfiguration('hcom.bridge').get('binaryPath', 'hcom')}\n` +
                `Project: ${vscode.workspace.getConfiguration('hcom.bridge').get('project', '(none)')}`);
        }));
    }
    registerChatParticipant(context) {
        const participant = vscode.chat.createChatParticipant('hcom.hcom', async (request, _context, response, _token) => {
            const prompt = request.prompt.trim();
            if (prompt.startsWith('list') || prompt === 'agents' || prompt === 'ls') {
                try {
                    const agents = await this.hcomClient.listAgents();
                    response.markdown(`**hcom active agents:**\n\n\`\`\`\n${agents}\n\`\`\``);
                }
                catch (err) {
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
                    response.markdown(`✅ Sent message to @${target}`);
                }
                catch (err) {
                    const msg = err instanceof Error ? err.message : String(err);
                    response.markdown(`❌ Failed: ${msg}`);
                }
                return;
            }
            response.markdown(`Usage:\n\n` +
                `- \`@hcom list\` — list active agents\n` +
                `- \`@hcom send @name message\` — send message to agent\n` +
                `- \`@hcom @name message\` — shorthand (auto-detect as send)\n` +
                `- \`@hcom status\` — show bridge status\n` +
                `- \`@hcom help\` — this help`);
        });
        participant.followupProvider = {
            provideFollowups(_result, _token) {
                return [
                    { prompt: 'list', label: 'List agents' },
                    { prompt: 'status', label: 'Bridge status' },
                ];
            }
        };
        context.subscriptions.push(participant);
    }
    forwardToJetski(sender, text) {
        const config = vscode.workspace.getConfiguration('hcom.bridge');
        const autoSend = config.get('autoSendToJetski', true);
        if (!autoSend)
            return;
        const ext = this.getAntigravityExt();
        if (!ext) {
            this.updateStatusBar('no antigravity API');
            return;
        }
        try {
            const message = `**[hcom] @${sender}**\n\n${text}`;
            ext.sendToAgentPanel({ message, autoSend: true });
        }
        catch {
            this.updateStatusBar('send failed');
        }
    }
    getAntigravityExt() {
        try {
            const win = vscode.window;
            if (win.antigravityExtensibility && typeof win.antigravityExtensibility.sendToAgentPanel === 'function') {
                return win.antigravityExtensibility;
            }
        }
        catch { }
        return undefined;
    }
    updateStatusBar(status) {
        const icons = {
            'active': '$(broadcast)',
            'listening': '$(broadcast)',
            'connected': '$(broadcast)',
            'connecting': '$(sync~spin)',
            'reconnecting': '$(sync~spin)',
            'error': '$(error)',
            'exited': '$(debug-disconnect)',
            'initializing': '$(loading~spin)',
        };
        const icon = Object.entries(icons).find(([k]) => status.startsWith(k))?.[1] || '$(question)';
        this.statusBarItem.text = `${icon} hcom`;
        this.statusBarItem.backgroundColor = status.includes('error') || status.startsWith('exited')
            ? new vscode.ThemeColor('statusBarItem.errorBackground')
            : undefined;
    }
    dispose() {
        this.statusBarItem.dispose();
        for (const d of this.disposables) {
            d.dispose();
        }
        this.disposables = [];
    }
}
exports.JetskiBridge = JetskiBridge;
