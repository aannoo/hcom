import * as vscode from 'vscode';
import { HcomClient } from './hcomClient';

/// <reference path="./antigravity.d.ts" />

export class JetskiBridge implements vscode.Disposable {
    private hcomClient: HcomClient;
    private statusBarItem: vscode.StatusBarItem;
    private disposables: vscode.Disposable[] = [];

    constructor(hcomClient: HcomClient) {
        this.hcomClient = hcomClient;
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
                vscode.window.showInformationMessage(
                    `hcom Bridge: ${status}\n\n` +
                    `Binary: ${vscode.workspace.getConfiguration('hcom.bridge').get('binaryPath', 'hcom')}\n` +
                    `Project: ${vscode.workspace.getConfiguration('hcom.bridge').get('project', '(none)')}`
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
        if (!autoSend) return;

        const ext = this.getAntigravityExt();
        if (!ext) {
            this.updateStatusBar('no antigravity API');
            return;
        }

        try {
            const message = `**[hcom] @${sender}**\n\n${text}`;
            ext.sendToAgentPanel({ message, autoSend: true });
        } catch {
            this.updateStatusBar('send failed');
        }
    }

    private getAntigravityExt(): typeof antigravityExtensibility | undefined {
        try {
            const win = vscode.window as { antigravityExtensibility?: typeof antigravityExtensibility };
            if (win.antigravityExtensibility && typeof win.antigravityExtensibility.sendToAgentPanel === 'function') {
                return win.antigravityExtensibility;
            }
        } catch { }
        return undefined;
    }

    private updateStatusBar(status: string): void {
        const icons: Record<string, string> = {
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

    dispose(): void {
        this.statusBarItem.dispose();
        for (const d of this.disposables) {
            d.dispose();
        }
        this.disposables = [];
    }
}
