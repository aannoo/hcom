declare namespace antigravityExtensibility {
    function sendToAgentPanel(options: {
        message?: string;
        files?: { uri: import('vscode').Uri; lineRange?: [number, number] }[];
        autoSend?: boolean;
    }): void;
    function refreshMcpServers(): void;
    function writeMcpConfig(pluginName: string, content: string): void;
    function getMcpConfig(pluginName: string): string | undefined;
}
