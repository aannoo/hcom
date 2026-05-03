"use strict";
Object.defineProperty(exports, "__esModule", { value: true });
exports.activate = activate;
exports.deactivate = deactivate;
const hcomClient_1 = require("./hcomClient");
const jetskiBridge_1 = require("./jetskiBridge");
let hcomClient;
let jetskiBridge;
function activate(context) {
    hcomClient = new hcomClient_1.HcomClient();
    jetskiBridge = new jetskiBridge_1.JetskiBridge(hcomClient);
    context.subscriptions.push(hcomClient);
    context.subscriptions.push(jetskiBridge);
    hcomClient.start();
    jetskiBridge.activate(context);
    console.log('hcom Bridge activated');
}
function deactivate() {
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
