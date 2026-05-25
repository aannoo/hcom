import * as vscode from 'vscode';
import * as fs from 'fs';
import { HcomClient } from './hcomClient';
import { JetskiBridge } from './jetskiBridge';

let hcomClient: HcomClient | undefined;
let jetskiBridge: JetskiBridge | undefined;

function logToFile(level: string, msg: string): void {
    try {
        const home = process.env.HOME || process.env.USERPROFILE || '';
        const logDir = path.join(home, '.hcom', 'extensions');
        fs.mkdirSync(logDir, { recursive: true });
        const ts = new Date().toISOString();
        fs.appendFileSync(path.join(logDir, 'hcom-bridge.log'), `[${ts}] [${level}] [ext] ${msg}\n`);
    } catch { }
}

function deriveAgentName(): string | null {
    const folder = vscode.workspace.workspaceFolders?.[0];
    if (!folder) return null;
    const base = folder.name.replace(/[^a-zA-Z0-9_-]/g, '-').replace(/-+/g, '-').replace(/^-|-$/g, '');
    return base ? `antigravity-${base}` : null;
}

function deriveProjectName(): string {
    const folder = vscode.workspace.workspaceFolders?.[0];
    if (!folder) return '';
    return folder.name.replace(/[^a-zA-Z0-9_-]/g, '-').replace(/-+/g, '-').replace(/^-|-$/g, '');
}

export async function activate(context: vscode.ExtensionContext): Promise<void> {
    const name = deriveAgentName();
    hcomClient = new HcomClient();
    jetskiBridge = new JetskiBridge(hcomClient, name);
    context.subscriptions.push(hcomClient);
    context.subscriptions.push(jetskiBridge);

    if (name) {
        jetskiBridge.updateStatusBar('registering');
        const project = deriveProjectName();
        await hcomClient.startAgent(name, project);
    }

    hcomClient.start();
    jetskiBridge.activate(context);

    if (name) {
        jetskiBridge.updateStatusBar(`registered as ${name}`);
    } else {
        jetskiBridge.updateStatusBar('no agent (no workspace)');
    }
    logToFile('INFO', `activated${name ? ` as agent "${name}"` : ''}`);
}

export async function deactivate(): Promise<void> {
    if (hcomClient) {
        await hcomClient.stopAgent();
        hcomClient.dispose();
        hcomClient = undefined;
    }
    if (jetskiBridge) {
        jetskiBridge.dispose();
        jetskiBridge = undefined;
    }
    logToFile('INFO', 'deactivated');
}
