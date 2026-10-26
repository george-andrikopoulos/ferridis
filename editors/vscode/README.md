# Ferridis — VS Code extension

Brings Ferridis-connected capabilities (native adapters and any wrapped MCP
servers) into VS Code's native MCP runtime, so they appear as tools in Copilot
Chat. Coexists with any other MCP servers you have configured; it does not
replace existing MCP plumbing.

There are two ways to connect, chosen by which setting is filled in:

| Mode | Setting | What happens |
|---|---|---|
| **Service** (recommended) | `ferridis.serverUrl` | VS Code connects to a `ferridis-mcp-server` that is **already running** with `--http-bind`, for example as a Windows service. One federation is shared with every other client of that service. |
| **Stdio** | `ferridis.serverPath` | VS Code launches its **own** `ferridis-mcp-server` over stdio and stops it with the window. |

If both are set, `ferridis.serverUrl` wins and the stdio entry is removed, so
the two never run side by side.

## What you need

- VS Code 1.101+ (the release that added MCP server definition providers).
- **Service mode:** a running `ferridis-mcp-server --http-bind 127.0.0.1:<port>`
  and the bearer token it was started with (`--bearer-token` or
  `FERRIDIS_MCP_BEARER`).
- **Stdio mode:** a built `ferridis-mcp-server` binary and an adapters config:
  ```bash
  cd rust && cargo build --release -p ferridis-mcp-server
  # binary at: rust/target/release/ferridis-mcp-server
  ```
  The adapters config lists the capabilities to expose. Each entry can be a
  native Ferridis adapter, an MCP SSE server, or an MCP stdio server.
  Defaults to `~/.ferridis/adapters.json` when the setting is empty:
  ```bash
  cat > ~/.ferridis/adapters.json <<'EOF'
  [
    {
      "capability": "ferridis://public.ferridis.io/ferridis/fs@v1",
      "manifestUrl": "http://127.0.0.1:7821/manifest.json",
      "baseUrl": "http://127.0.0.1:7821/"
    }
  ]
  EOF
  ```
  and at least one running adapter, e.g.
  `cargo run --release -p ferridis-adapter-fs --example serve-fs`.

## Configure — service mode

1. In Settings (JSON):
   ```jsonc
   {
     "ferridis.serverUrl": "http://127.0.0.1:7820/mcp"
   }
   ```
   Plain `http` is accepted only for a loopback address (`127.0.0.1`,
   `localhost`, `[::1]`). Anything else must be `https`, because the token
   travels in a request header.
2. Run **Ferridis: Set Service Token** and paste the service's bearer token.
   It is kept in VS Code's secret storage and attached only when VS Code
   starts the connection; it is never written to `settings.json`.

If the token is missing, the server is not started and VS Code offers to set
it.

## Configure — stdio mode

```jsonc
{
  "ferridis.serverPath": "/absolute/path/to/rust/target/release/ferridis-mcp-server",
  "ferridis.adaptersConfig": "/absolute/path/to/.ferridis/adapters.json"
}
```

Optional: `ferridis.keychainNamespace`, the OS keychain service name used for
credential storage (default `"ferridis"`).

A stdio server runs as you. An adapter that needs more privilege (for example
one that requires elevation on Windows) will be skipped there, while the same
adapter works under a service running with that privilege.

## Commands

| Command | What it does |
|---|---|
| `Ferridis: Configure` | Open the extension settings. |
| `Ferridis: Show Logs` | Open the Ferridis output channel. |
| `Ferridis: Set Service Token` | Store the bearer token for service mode in VS Code's secret storage. |
| `Ferridis: Clear Service Token` | Remove the stored token. |

## Building and testing

```bash
cd editors/vscode
npm install
npm test                # settings and token parsing (node --test)
npm run typecheck       # tsc --noEmit
npm run package         # bundle with esbuild and produce ferridis.vsix
code --install-extension ferridis.vsix
```

`npm test` covers the rules in `src/connection.ts`: which mode the settings
select, which URLs are refused, and which tokens are refused. What it does not
cover is the VS Code side (the provider, secret storage, the settings write),
which is still verified by hand.

For an iterative dev loop, open `editors/vscode/` in VS Code and hit `F5` to
launch an Extension Development Host with the extension live.

## Reconnecting after a server rebuild

Copilot pins tool schemas at chat-session start. After a server change (a new
adapter, a schema change, a redeployed service), run `Developer: Reload Window`
so Copilot rediscovers the tool definitions. In stdio mode the reload also
restarts the server process.
