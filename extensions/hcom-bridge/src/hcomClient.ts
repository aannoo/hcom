import * as cp from 'child_process';
import * as fs from 'fs';
import * as path from 'path';
import * as vscode from 'vscode';

type MessageHandler = (sender: string, text: string) => void;

/** Write a log line to ~/.hcom/extensions/hcom-bridge.log */
function logToFile(level: string, msg: string): void {
    try {
        const home = process.env.HOME || process.env.USERPROFILE || '';
        const logDir = path.join(home, '.hcom', 'extensions');
        fs.mkdirSync(logDir, { recursive: true });
        const ts = new Date().toISOString();
        fs.appendFileSync(path.join(logDir, 'hcom-bridge.log'), `[${ts}] [${level}] ${msg}\n`);
    } catch { /* ignore file errors */ }
}

/** Common locations to look for the hcom binary. */
const COMMON_HCOM_PATHS = [
    'hcom',
    path.join(process.env.HOME || '', '.local', 'bin', 'hcom'),
    path.join(process.env.HOME || '', '.cargo', 'bin', 'hcom'),
    '/usr/local/bin/hcom',
    '/opt/homebrew/bin/hcom',
];

function resolveBinaryPath(): string {
    const config = vscode.workspace.getConfiguration('hcom.bridge');
    const configured = config.get<string>('binaryPath', 'hcom');
    if (configured && configured !== 'hcom') {
        logToFile('INFO', `using configured binary path: ${configured}`);
        return configured;
    }
    for (const p of COMMON_HCOM_PATHS) {
        if (p === 'hcom') continue;
        if (fs.existsSync(p)) {
            logToFile('INFO', `resolved hcom binary: ${p}`);
            return p;
        }
    }
    logToFile('WARN', 'hcom binary not found at common paths, using "hcom" (PATH)');
    return 'hcom';
}

export class HcomClient implements vscode.Disposable {
    private process: cp.ChildProcess | null = null;
    private onMessageHandlers: MessageHandler[] = [];
    private onStatusHandlers: Array<(status: string) => void> = [];
    private restartTimeout: ReturnType<typeof setTimeout> | null = null;
    private _disposed = false;
    private _agentName: string | null = null;

    get isRunning(): boolean {
        return this.process !== null && !this.process.killed;
    }

    get agentName(): string | null {
        return this._agentName;
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

    /** Register as an hcom agent so other agents can send messages to us. */
    async startAgent(name: string, project?: string): Promise<void> {
        this._agentName = name;
        const binaryPath = resolveBinaryPath();
        this.emitStatus(`registering as ${name}`);
        try {
            await new Promise<void>((resolve, reject) => {
                cp.execFile(binaryPath, ['start', '--name', name], { timeout: 10000 }, (err, stdout) => {
                    if (err) reject(new Error(err.message));
                    else {
                        console.log(`hcom agent registered: ${name}`);
                        this.emitStatus(`registered as ${name}`);
                        resolve();
                    }
                });
            });
            // Set project isolation so agent appears in the right project
            if (project) {
                try {
                    await new Promise<void>((resolve, reject) => {
                        cp.execFile(binaryPath, ['config', '-i', name, 'project', project], { timeout: 5000 }, (err) => {
                            if (err) console.error('set project:', err.message);
                            else console.log(`project set to ${project} for ${name}`);
                            resolve();
                        });
                    });
                } catch { }
            }
        } catch (err) {
            const msg = err instanceof Error ? err.message : String(err);
            console.error('hcom start failed:', msg);
            this.emitStatus(`register error: ${msg}`);
        }
    }

    /** Unregister the hcom agent. */
    async stopAgent(): Promise<void> {
        const name = this._agentName;
        if (!name) return;
        this._agentName = null;
        this.emitStatus('unregistering');
        const binaryPath = resolveBinaryPath();
        try {
            await new Promise<void>((resolve, reject) => {
                cp.execFile(binaryPath, ['stop', name], { timeout: 10000 }, (err) => {
                    if (err) console.error('hcom stop:', err.message);
                    else console.log(`hcom agent unregistered: ${name}`);
                    resolve();
                });
            });
        } catch { }
    }

    private spawn(): void {
        if (this._disposed) return;
        this.kill();

        const binaryPath = resolveBinaryPath();
        const project = vscode.workspace.getConfiguration('hcom.bridge').get<string>('project', '');
        const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;

        const args = ['listen', '--json'];
        if (this._agentName) {
            args.push('--name', this._agentName);
        } else if (project) {
            args.push('--project', project);
        }

        const options: cp.SpawnOptions = {
            stdio: ['ignore', 'pipe', 'pipe'],
            detached: false,
        };

        logToFile('INFO', `spawning: ${binaryPath} ${args.join(' ')}`);
        this.emitStatus('connecting');
        const proc = cp.spawn(binaryPath, args, options);
        this.process = proc;

        let buffer = '';
        proc.stdout?.on('data', (data: Buffer) => {
            const text = data.toString();
            logToFile('DEBUG', `stdout: ${text.trim().slice(0, 200)}`);
            buffer += text;
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
                logToFile('INFO', `stderr: ${text.slice(0, 200)}`);
                this.emitStatus(`stderr: ${text}`);
            }
        });

        proc.on('error', (err) => {
            logToFile('ERROR', `spawn error: ${err.message}`);
            this.emitStatus(`error: ${err.message}`);
            this.scheduleRestart();
        });

        proc.on('exit', (code) => {
            logToFile('INFO', `process exited (code=${code})`);
            this.process = null;
            this.emitStatus(`exited (${code})`);
            if (!this._disposed) {
                this.scheduleRestart();
            }
        });

        this.emitStatus('listening');
        logToFile('INFO', `listen spawned successfully, pid=${proc.pid}`);
    }

    private parseBuffer(buffer: string, callback: (msg: string) => void): void {
        const lines = buffer.split('\n');
        for (const line of lines) {
            const trimmed = line.trim();
            if (trimmed.startsWith('{') && trimmed.endsWith('}')) {
                callback(trimmed);
            }
        }
    }

    private parseHcomMessage(line: string): { sender: string; text: string } | null {
        // JSON mode: {"from":"hiro","text":"5+9?"}
        try {
            const obj = JSON.parse(line);
            const from = obj.from;
            const text = obj.text;
            if (from && text) {
                return { sender: from, text };
            }
        } catch { }
        return null;
    }

    async send(target: string, message: string): Promise<string> {
        const binaryPath = resolveBinaryPath();
        const project = vscode.workspace.getConfiguration('hcom.bridge').get<string>('project', '');

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
        const binaryPath = resolveBinaryPath();
        const project = vscode.workspace.getConfiguration('hcom.bridge').get<string>('project', '');

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
