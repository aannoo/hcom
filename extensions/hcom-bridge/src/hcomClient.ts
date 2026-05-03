import * as cp from 'child_process';
import * as vscode from 'vscode';

type MessageHandler = (sender: string, text: string) => void;

export class HcomClient implements vscode.Disposable {
    private process: cp.ChildProcess | null = null;
    private onMessageHandlers: MessageHandler[] = [];
    private onStatusHandlers: Array<(status: string) => void> = [];
    private restartTimeout: ReturnType<typeof setTimeout> | null = null;
    private _disposed = false;

    get isRunning(): boolean {
        return this.process !== null && !this.process.killed;
    }

    onMessage(handler: MessageHandler): vscode.Disposable {
        this.onMessageHandlers.push(handler);
        return { dispose: () => this.offMessage(handler) };
    }

    onStatus(handler: (status: string) => void): vscode.Disposable {
        this.onStatusHandlers.push(handler);
        return { dispose: () => this.offStatus(handler) };
    }

    private offMessage(handler: MessageHandler): void {
        this.onMessageHandlers = this.onMessageHandlers.filter(h => h !== handler);
    }

    private offStatus(handler: (status: string) => void): void {
        this.onStatusHandlers = this.onStatusHandlers.filter(h => h !== handler);
    }

    private emitMessage(sender: string, text: string): void {
        for (const handler of this.onMessageHandlers) {
            try { handler(sender, text); } catch { }
        }
    }

    private emitStatus(status: string): void {
        for (const handler of this.onStatusHandlers) {
            try { handler(status); } catch { }
        }
    }

    start(): void {
        if (this._disposed) return;
        this.spawn();
    }

    private spawn(): void {
        if (this._disposed) return;
        this.kill();

        const config = vscode.workspace.getConfiguration('hcom.bridge');
        const binaryPath = config.get<string>('binaryPath', 'hcom');
        const project = config.get<string>('project', '');
        const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;

        const args = ['listen'];
        if (project) {
            args.push('--project', project);
        }
        if (workspaceRoot) {
            args.push('--cwd', workspaceRoot);
        }

        const options: cp.SpawnOptions = {
            stdio: ['ignore', 'pipe', 'pipe'],
            detached: false,
        };
        if (workspaceRoot) {
            options.cwd = workspaceRoot;
        }

        this.emitStatus('connecting');
        const proc = cp.spawn(binaryPath, args, options);
        this.process = proc;

        let buffer = '';
        proc.stdout?.on('data', (data: Buffer) => {
            buffer += data.toString();
            this.parseBuffer(buffer, msg => {
                buffer = buffer.slice(buffer.indexOf(msg) + msg.length + 1);
                const parsed = this.parseHcomMessage(msg);
                if (parsed) {
                    this.emitMessage(parsed.sender, parsed.text);
                }
            });
        });

        proc.stderr?.on('data', (data: Buffer) => {
            const text = data.toString().trim();
            if (text) {
                this.emitStatus(`stderr: ${text}`);
            }
        });

        proc.on('error', (err) => {
            this.emitStatus(`error: ${err.message}`);
            this.scheduleRestart();
        });

        proc.on('exit', (code) => {
            this.process = null;
            this.emitStatus(`exited (${code})`);
            if (!this._disposed) {
                this.scheduleRestart();
            }
        });

        this.emitStatus('listening');
    }

    private parseBuffer(buffer: string, callback: (msg: string) => void): void {
        const lines = buffer.split('\n');
        for (const line of lines) {
            if (line.includes('[hcom]') || line.includes('<hcom>')) {
                callback(line);
            }
        }
    }

    private parseHcomMessage(line: string): { sender: string; text: string } | null {
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

    async send(target: string, message: string): Promise<string> {
        const config = vscode.workspace.getConfiguration('hcom.bridge');
        const binaryPath = config.get<string>('binaryPath', 'hcom');
        const project = config.get<string>('project', '');

        const args = ['send', `@${target}`, '--', message];
        if (project) {
            args.push('--project', project);
        }

        return new Promise((resolve, reject) => {
            cp.execFile(binaryPath, args, { timeout: 10000 }, (err, stdout, stderr) => {
                if (err) reject(new Error(stderr || err.message));
                else resolve(stdout.trim());
            });
        });
    }

    async listAgents(): Promise<string> {
        const config = vscode.workspace.getConfiguration('hcom.bridge');
        const binaryPath = config.get<string>('binaryPath', 'hcom');
        const project = config.get<string>('project', '');

        const args = ['list', '--names'];
        if (project) {
            args.push('--project', project);
        }

        return new Promise((resolve, reject) => {
            cp.execFile(binaryPath, args, { timeout: 5000 }, (err, stdout) => {
                if (err) reject(err);
                else resolve(stdout.trim());
            });
        });
    }

    private scheduleRestart(): void {
        if (this._disposed) return;
        if (this.restartTimeout) {
            clearTimeout(this.restartTimeout);
        }
        this.restartTimeout = setTimeout(() => this.spawn(), 3000);
        this.emitStatus('reconnecting in 3s...');
    }

    private kill(): void {
        if (this.restartTimeout) {
            clearTimeout(this.restartTimeout);
            this.restartTimeout = null;
        }
        if (this.process && !this.process.killed) {
            try {
                this.process.kill();
            } catch { }
        }
        this.process = null;
    }

    dispose(): void {
        this._disposed = true;
        this.kill();
        this.onMessageHandlers = [];
        this.onStatusHandlers = [];
    }
}
