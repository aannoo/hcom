import * as vscode from 'vscode';
import { HcomClient } from './hcomClient';
import { JetskiBridge } from './jetskiBridge';

let hcomClient: HcomClient | undefined;
let jetskiBridge: JetskiBridge | undefined;

export function activate(context: vscode.ExtensionContext): void {
    hcomClient = new HcomClient();
    jetskiBridge = new JetskiBridge(hcomClient);

    context.subscriptions.push(hcomClient);
    context.subscriptions.push(jetskiBridge);

    hcomClient.start();
    jetskiBridge.activate(context);

    console.log('hcom Bridge activated');
}

export function deactivate(): void {
    if (hcomClient) {
        hcomClient.dispose();
        hcomClient = undefined;
    }
    if (jetskiBridge) {
        jetskiBridge.dispose();
        jetskiBridge = undefined;
    }
    console.log('hcom Bridge deactivated');
}
