// Ferridis VS Code extension v0.3 — entry point.
//
// Two ways to reach Ferridis, chosen by settings (see connection.ts):
//
//   service  ferridis.serverUrl is set. VS Code connects to an already-running
//            ferridis-mcp-server over Streamable HTTP, through an MCP server
//            definition provider. The bearer token lives in VS Code's secret
//            storage and is attached only when VS Code starts the server, so
//            it is never written to settings.json.
//   stdio    only ferridis.serverPath is set (the v0.2 behaviour). The
//            extension writes an mcp.servers entry and VS Code launches a
//            private ferridis-mcp-server over stdio.
//
// Re-syncs whenever the Ferridis settings or the stored token change.

import * as os from "os";
import * as path from "path";
import * as vscode from "vscode";
import {
    describeConnectionError,
    describeTokenError,
    parseBearerToken,
    parseConnection,
} from "./connection.ts";
import type { Connection } from "./connection.ts";

/** Key of the v0.2 stdio entry in the `mcp.servers` setting. */
const MCP_SERVER_KEY = "ferridis";
/** Must match `contributes.mcpServerDefinitionProviders[].id` in package.json. */
const PROVIDER_ID = "ferridis.service";
const TOKEN_SECRET_KEY = "ferridis.serviceToken";
const SERVER_LABEL = "Ferridis";

export async function activate(context: vscode.ExtensionContext): Promise<void> {
    const output = vscode.window.createOutputChannel("Ferridis");
    context.subscriptions.push(output);
    output.appendLine(`[ferridis] activate (vscode=${vscode.version})`);

    const definitionsChanged = new vscode.EventEmitter<void>();
    context.subscriptions.push(definitionsChanged);

    context.subscriptions.push(
        vscode.lm.registerMcpServerDefinitionProvider(PROVIDER_ID, {
            onDidChangeMcpServerDefinitions: definitionsChanged.event,
            provideMcpServerDefinitions: () => {
                const connection = readConnection(output);
                if (connection?.kind !== "service") {
                    return [];
                }
                return [new vscode.McpHttpServerDefinition(SERVER_LABEL, vscode.Uri.parse(connection.url.href))];
            },
            resolveMcpServerDefinition: async (server) => {
                if (!(server instanceof vscode.McpHttpServerDefinition)) {
                    return server;
                }
                const stored = await context.secrets.get(TOKEN_SECRET_KEY);
                const token = parseBearerToken(stored ?? "");
                if (!token.ok) {
                    output.appendLine(`[ferridis] not starting: no usable service token (${describeTokenError(token.error)})`);
                    void offerToSetToken();
                    return undefined;
                }
                server.headers = { ...server.headers, Authorization: `Bearer ${token.value.value}` };
                return server;
            },
        }),
    );

    await sync(output);

    context.subscriptions.push(
        vscode.workspace.onDidChangeConfiguration(async (e) => {
            if (e.affectsConfiguration("ferridis")) {
                output.appendLine("[ferridis] settings changed; re-syncing");
                await sync(output);
                definitionsChanged.fire();
            }
        }),
        context.secrets.onDidChange((e) => {
            if (e.key === TOKEN_SECRET_KEY) {
                output.appendLine("[ferridis] service token changed");
                definitionsChanged.fire();
            }
        }),
        vscode.commands.registerCommand("ferridis.configure", () => {
            void vscode.commands.executeCommand("workbench.action.openSettings", "ferridis");
        }),
        vscode.commands.registerCommand("ferridis.showLogs", () => {
            output.show(true);
        }),
        vscode.commands.registerCommand("ferridis.setServiceToken", async () => {
            const entered = await vscode.window.showInputBox({
                title: "Ferridis service token",
                prompt: "The bearer token the Ferridis service was started with. Stored in VS Code's secret storage.",
                password: true,
                ignoreFocusOut: true,
            });
            if (entered === undefined) {
                return;
            }
            const token = parseBearerToken(entered);
            if (!token.ok) {
                void vscode.window.showErrorMessage(`Ferridis: ${describeTokenError(token.error)}`);
                return;
            }
            await context.secrets.store(TOKEN_SECRET_KEY, token.value.value);
            void vscode.window.showInformationMessage("Ferridis: service token saved.");
        }),
        vscode.commands.registerCommand("ferridis.clearServiceToken", async () => {
            await context.secrets.delete(TOKEN_SECRET_KEY);
            void vscode.window.showInformationMessage("Ferridis: service token removed.");
        }),
    );
}

export function deactivate(): void {}

/** Parse the settings; on a refused value, say why and return undefined. */
function readConnection(output: vscode.OutputChannel): Connection | undefined {
    const cfg = vscode.workspace.getConfiguration("ferridis");
    const parsed = parseConnection(
        {
            serverUrl: cfg.get<string>("serverUrl", ""),
            serverPath: cfg.get<string>("serverPath", ""),
            adaptersConfig: cfg.get<string>("adaptersConfig", ""),
            keychainNamespace: cfg.get<string>("keychainNamespace", "ferridis"),
        },
        path.join(os.homedir(), ".ferridis", "adapters.json"),
    );
    if (!parsed.ok) {
        const message = describeConnectionError(parsed.error);
        output.appendLine(`[ferridis] ${message}`);
        void vscode.window.showErrorMessage(`Ferridis: ${message}`);
        return undefined;
    }
    return parsed.value;
}

/** Bring the `mcp.servers` setting in line with the current connection. */
async function sync(output: vscode.OutputChannel): Promise<void> {
    const connection = readConnection(output);
    if (connection === undefined) {
        return;
    }
    switch (connection.kind) {
        case "service":
            // The provider serves this mode. A v0.2 stdio entry left behind
            // would start a second copy of the federation beside it.
            await removeStdioEntry(output, "ferridis.serverUrl is set");
            output.appendLine(`[ferridis] service mode: ${connection.url.href}`);
            return;
        case "stdio":
            await writeStdioEntry(output, connection);
            return;
        case "unconfigured":
            await removeStdioEntry(output, "neither ferridis.serverUrl nor ferridis.serverPath is set");
            void vscode.window
                .showWarningMessage(
                    "Ferridis: set ferridis.serverUrl (a running Ferridis service) or ferridis.serverPath (the ferridis-mcp-server binary) to enable tool discovery.",
                    "Open Settings",
                )
                .then((action) => {
                    if (action === "Open Settings") {
                        void vscode.commands.executeCommand("workbench.action.openSettings", "ferridis");
                    }
                });
            return;
    }
}

async function writeStdioEntry(
    output: vscode.OutputChannel,
    connection: Extract<Connection, { kind: "stdio" }>,
): Promise<void> {
    const mcpCfg = vscode.workspace.getConfiguration("mcp");
    const servers = mcpCfg.get<Record<string, unknown>>("servers") ?? {};
    const updated = {
        ...servers,
        [MCP_SERVER_KEY]: {
            type: "stdio",
            command: connection.serverPath,
            args: [
                "--adapters-config",
                connection.adaptersConfig,
                "--keychain-namespace",
                connection.keychainNamespace,
            ],
        },
    };
    await mcpCfg.update("servers", updated, vscode.ConfigurationTarget.Global);
    output.appendLine(
        `[ferridis] registered in mcp.servers: ${connection.serverPath} --adapters-config ${connection.adaptersConfig}`,
    );
}

async function removeStdioEntry(output: vscode.OutputChannel, reason: string): Promise<void> {
    const mcpCfg = vscode.workspace.getConfiguration("mcp");
    const servers = mcpCfg.get<Record<string, unknown>>("servers") ?? {};
    if (!(MCP_SERVER_KEY in servers)) {
        return;
    }
    const updated = { ...servers };
    delete updated[MCP_SERVER_KEY];
    await mcpCfg.update("servers", updated, vscode.ConfigurationTarget.Global);
    output.appendLine(`[ferridis] removed the stdio entry from mcp.servers (${reason})`);
}

async function offerToSetToken(): Promise<void> {
    const action = await vscode.window.showErrorMessage(
        "Ferridis: no service token is stored, so the Ferridis server was not started.",
        "Set Token",
    );
    if (action === "Set Token") {
        await vscode.commands.executeCommand("ferridis.setServiceToken");
    }
}
