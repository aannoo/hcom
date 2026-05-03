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
exports.HcomClient = void 0;
const cp = __importStar(require("child_process"));
const vscode = __importStar(require("vscode"));
class HcomClient {
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
        this.onMessageHandlers = this.onMessageHandlers.filter(h => h !== handler);
    }
    offStatus(handler) {
        this.onStatusHandlers = this.onStatusHandlers.filter(h => h !== handler);
    }
    emitMessage(sender, text) {
        for (const handler of this.onMessageHandlers) {
            try {
                handler(sender, text);
            }
            catch { }
        }
    }
    emitStatus(status) {
        for (const handler of this.onStatusHandlers) {
            try {
                handler(status);
            }
            catch { }
        }
    }
    start() {
        if (this._disposed)
            return;
        this.spawn();
    }
    spawn() {
        if (this._disposed)
            return;
        this.kill();
        const config = vscode.workspace.getConfiguration('hcom.bridge');
        const binaryPath = config.get('binaryPath', 'hcom');
        const project = config.get('project', '');
        const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
        const args = ['listen'];
        if (project) {
            args.push('--project', project);
        }
        if (workspaceRoot) {
            args.push('--cwd', workspaceRoot);
        }
        const options = {
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
        proc.stdout?.on('data', (data) => {
            buffer += data.toString();
            this.parseBuffer(buffer, msg => {
                buffer = buffer.slice(buffer.indexOf(msg) + msg.length + 1);
                const parsed = this.parseHcomMessage(msg);
                if (parsed) {
                    this.emitMessage(parsed.sender, parsed.text);
                }
            });
        });
        proc.stderr?.on('data', (data) => {
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
    parseBuffer(buffer, callback) {
        const lines = buffer.split('\n');
        for (const line of lines) {
            if (line.includes('[hcom]') || line.includes('<hcom>')) {
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
        const config = vscode.workspace.getConfiguration('hcom.bridge');
        const binaryPath = config.get('binaryPath', 'hcom');
        const project = config.get('project', '');
        const args = ['send', `@${target}`, '--', message];
        if (project) {
            args.push('--project', project);
        }
        return new Promise((resolve, reject) => {
            cp.execFile(binaryPath, args, { timeout: 10000 }, (err, stdout, stderr) => {
                if (err)
                    reject(new Error(stderr || err.message));
                else
                    resolve(stdout.trim());
            });
        });
    }
    async listAgents() {
        const config = vscode.workspace.getConfiguration('hcom.bridge');
        const binaryPath = config.get('binaryPath', 'hcom');
        const project = config.get('project', '');
        const args = ['list', '--names'];
        if (project) {
            args.push('--project', project);
        }
        return new Promise((resolve, reject) => {
            cp.execFile(binaryPath, args, { timeout: 5000 }, (err, stdout) => {
                if (err)
                    reject(err);
                else
                    resolve(stdout.trim());
            });
        });
    }
    scheduleRestart() {
        if (this._disposed)
            return;
        if (this.restartTimeout) {
            clearTimeout(this.restartTimeout);
        }
        this.restartTimeout = setTimeout(() => this.spawn(), 3000);
        this.emitStatus('reconnecting in 3s...');
    }
    kill() {
        if (this.restartTimeout) {
            clearTimeout(this.restartTimeout);
            this.restartTimeout = null;
        }
        if (this.process && !this.process.killed) {
            try {
                this.process.kill();
            }
            catch { }
        }
        this.process = null;
    }
    dispose() {
        this._disposed = true;
        this.kill();
        this.onMessageHandlers = [];
        this.onStatusHandlers = [];
    }
}
exports.HcomClient = HcomClient;
