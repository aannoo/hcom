import * as vscode from 'vscode';
import * as fs from 'fs';
import * as path from 'path';
import { HcomClient } from './hcomClient';

/// <reference path="./antigravity.d.ts" />

/** Write a log line to ~/.hcom/extensions/hcom-bridge.log */
function logToFile(level: string, msg: string): void {
    try {
        const home = process.env.HOME || process.env.USERPROFILE || '';
        const logDir = path.join(home, '.hcom', 'extensions');
        fs.mkdirSync(logDir, { recursive: true });
        const ts = new Date().toISOString();
        fs.appendFileSync(path.join(logDir, 'hcom-bridge.log'), `[${ts}] [${level}] [jetski] ${msg}\n`);
    } catch { /* ignore */ }
}

export class JetskiBridge implements vscode.Disposable {
    private hcomClient: HcomClient;
    private statusBarItem: vscode.StatusBarItem;
    private disposables: vscode.Disposable[] = [];
    private _agentName: string | null = null;
    private _conversationStarted = false;
    private _msgCount = 0;

    constructor(hcomClient: HcomClient, agentName: string | null, private _projectName: string = '') {
        this.hcomClient = hcomClient;
        this._agentName = agentName;
        this.statusBarItem = vscode.window.createStatusBarItem(
            vscode.StatusBarAlignment.Left, 100
        );
        this.statusBarItem.command = 'hcom.showStatus';
        this.statusBarItem.tooltip = 'hcom Bridge — click for status';
        this.statusBarItem.show();
        this.updateStatusBar('initializing');
    }

    activate(context: vscode.ExtensionContext): void {
        this.registerListeners();
        this.registerCommands(context);
        this.registerChatParticipant(context);
        this.updateStatusBar('active');
    }

    private registerListeners(): void {
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

    private registerCommands(context: vscode.ExtensionContext): void {
        context.subscriptions.push(
            vscode.commands.registerCommand('hcom.listAgents', async () => {
                try {
                    const agents = await this.hcomClient.listAgents();
                    vscode.window.showInformationMessage(`hcom agents:\n${agents}`);
                } catch (err: unknown) {
                    const msg = err instanceof Error ? err.message : String(err);
                    vscode.window.showErrorMessage(`Failed to list agents: ${msg}`);
                }
            })
        );

        context.subscriptions.push(
            vscode.commands.registerCommand('hcom.sendMessage', async () => {
                const target = await vscode.window.showInputBox({
                    prompt: 'Target agent name',
                    placeHolder: 'e.g. moto, sora, yuri'
                });
                if (!target) return;

                const message = await vscode.window.showInputBox({
                    prompt: `Message to @${target}`,
                    placeHolder: 'Enter your message'
                });
                if (!message) return;

                try {
                    const result = await this.hcomClient.send(target, message);
                    vscode.window.showInformationMessage(`Sent: ${result}`);
                } catch (err: unknown) {
                    const msg = err instanceof Error ? err.message : String(err);
                    vscode.window.showErrorMessage(`Send failed: ${msg}`);
                }
            })
        );

        context.subscriptions.push(
            vscode.commands.registerCommand('hcom.showStatus', () => {
                const running = this.hcomClient.isRunning;
                const status = running ? 'Connected' : 'Disconnected';
                const httpPort = vscode.workspace.getConfiguration('antigravityBridge').get<number>('httpPort', 5000);
                const hasAutomation = running; // we'll detect this at runtime
                vscode.window.showInformationMessage(
                    `hcom Bridge: ${status}\n\n` +
                    `Agent: ${this._agentName || '(none)'}\n` +
                    `Binary: ${vscode.workspace.getConfiguration('hcom.bridge').get('binaryPath', 'hcom')}\n` +
                    `Project: ${vscode.workspace.getConfiguration('hcom.bridge').get('project', '(none)')}\n` +
                    `Automation API: http://localhost:${httpPort}`
                );
            })
        );
    }

    private registerChatParticipant(context: vscode.ExtensionContext): void {
        const participant = vscode.chat.createChatParticipant('hcom.hcom', async (
            request: vscode.ChatRequest,
            _context: vscode.ChatContext,
            response: vscode.ChatResponseStream,
            _token: vscode.CancellationToken
        ) => {
            const prompt = request.prompt.trim();

            if (prompt.startsWith('list') || prompt === 'agents' || prompt === 'ls') {
                try {
                    const agents = await this.hcomClient.listAgents();
                    response.markdown(`**hcom active agents:**\n\n\`\`\`\n${agents}\n\`\`\``);
                } catch (err: unknown) {
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
                } catch (err: unknown) {
                    const msg = err instanceof Error ? err.message : String(err);
                    response.markdown(`❌ Failed: ${msg}`);
                }
                return;
            }

            response.markdown(
                `Usage:\n\n` +
                `- \`@hcom list\` — list active agents\n` +
                `- \`@hcom send @name message\` — send message to agent\n` +
                `- \`@hcom @name message\` — shorthand (auto-detect as send)\n` +
                `- \`@hcom status\` — show bridge status\n` +
                `- \`@hcom help\` — this help`
            );
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

    private forwardToJetski(sender: string, text: string): void {
        const config = vscode.workspace.getConfiguration('hcom.bridge');
        const autoSend = config.get<boolean>('autoSendToJetski', true);
        if (!autoSend) { logToFile('INFO', `autoSend disabled`); return; }
        this.sendToAgentPanel(sender, text);
    }

    /** Use native Antigravity commands to send messages to the agent panel. */
    private async sendToAgentPanel(sender: string, text: string): Promise<void> {
        logToFile('INFO', `sending msg from ${sender} to agent panel`);
        try {
            if (!this._conversationStarted) {
                try {
                    await vscode.commands.executeCommand('antigravity.startNewConversation');
                    this._conversationStarted = true;
                    logToFile('INFO', 'new conversation started');
                    await new Promise(r => setTimeout(r, 2000));
                } catch (e) {
                    logToFile('WARN', `startNewConversation failed: ${e}`);
                }
            }

            this._msgCount++;
            const withInstructions = this._msgCount === 1 || this._msgCount % 15 === 0;

            const agentName = this._agentName || 'antigravity';
            const instructions = withInstructions
                ? `You are connected to hcom as "${agentName}".\n\
Respond using terminal: hcom send @${sender} --name ${agentName} -- "your response"\n\n` : '';

            await vscode.commands.executeCommand(
                'antigravity.sendPromptToAgentPanel',
                `${instructions}**[hcom message from @${sender}]**\n\n${text}`
            );
            logToFile('INFO', `msg #${this._msgCount} sent${withInstructions ? ' + instructions' : ''}`);
            this.updateStatusBar(`sent to antigravity agent`);
        } catch (e) {
            logToFile('ERROR', `send to agent panel failed: ${e}`);
            this.updateStatusBar('send failed');
        }
    }

    /** Update the status bar display. Public so extension.ts can show registration state. */
    updateStatusBar(status: string): void {
        const icons: Record<string, string> = {
            'active': '$(broadcast)',
            'listening': '$(broadcast)',
            'connected': '$(broadcast)',
            'registered': '$(broadcast)',
            'connecting': '$(sync~spin)',
            'reconnecting': '$(sync~spin)',
            'reconnecting in 3s': '$(sync~spin)',
            'registering': '$(sync~spin)',
            'error': '$(error)',
            'no antigravity API': '$(warning)',
            'exited': '$(debug-disconnect)',
            'initializing': '$(loading~spin)',
            'no agent': '$(debug-disconnect)',
        };
        const icon = Object.entries(icons).find(([k]) => status.startsWith(k))?.[1] || '$(question)';
        const label = this._agentName ? ` ${this._agentName}` : '';
        this.statusBarItem.text = `${icon} hcom${label}`;
        this.statusBarItem.tooltip = `hcom Bridge — ${status}${this._agentName ? `\nAgent: ${this._agentName}` : ''}`;
        this.statusBarItem.backgroundColor = status.includes('error') || status.startsWith('exited')
            ? new vscode.ThemeColor('statusBarItem.errorBackground')
            : undefined;
    }

    dispose(): void {
        this.statusBarItem.dispose();
        for (const d of this.disposables) {
            d.dispose();
        }
        this.disposables = [];
    }
}
