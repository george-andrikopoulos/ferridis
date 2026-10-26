# Ferridis

**An opinionated, type-driven take on agent connectivity — designed to live alongside MCP.**

Ferridis is a reference design and Rust implementation for the *experience* layer above the wire protocols that connect AI assistants to the world. Users authorize *connections* — durable, consent-based links to services — instead of installing and running server processes. Capabilities are discovered through a federated mesh with lazy-loaded schemas. Events flow back from the world over bidirectional channels. The browser stays as a universal fallback for anything not yet wired up.

The user sees one thing: *I connected my calendar. The assistant uses it.*

## Relationship to MCP

Ferridis is not a replacement for MCP. In December 2025 MCP was donated to the **Agentic AI Foundation** (AAIF) — a Linux Foundation directed fund co-founded by Anthropic, Block, and OpenAI. Google's A2A, an open standard for agent-to-agent communication, is a complementary protocol in the same ecosystem. The wire protocol for connecting AI to tools is now an industry-coalition standard, which is good for the ecosystem and good for users.

Ferridis lives one layer above that standard. It is the opinionated take on the parts the wire protocol deliberately leaves to implementers: connections instead of servers, two-stage manifest/schema discovery, a three-tier federated mesh, a controlled intent vocabulary, browser-as-tier-3 inside one protocol, and a type-driven Rust reference with stronger compile-time guarantees than the existing TypeScript and Python references. MCP/A2A interoperability — Ferridis-connected capabilities published as MCP tools by `ferridis-mcp-server`, and MCP servers consumed as `Tier::Native` Ferridis capabilities by `ferridis-client` — ships in both directions today.

## Why

The full argument is in the [manifesto](./manifesto.md). The short version: AI's bottleneck is no longer model intelligence, it is integration. The wire is solved. The experience around it is not.

## How it's structured

Six layers, web-native throughout:

![Six-layer architecture](./diagrams/layers.svg)

Full architecture: [architecture.md](./architecture.md). Wire protocol sketch: [wire-protocol.md](./wire-protocol.md). Resolved design tensions: [architecture-frictions.md](./architecture-frictions.md).

## Status

The design is locked. The Rust reference implementation is at **v0.6** of the protocol — all six layers running end-to-end with full MCP/A2A interoperability in both directions.

What runs today:

- **The full six-layer stack end-to-end.** Connections (OAuth 2.0 + PKCE, OS-keychain wallet via `keyring`), capability mesh (manifest + lazy schema fetch with TTL cache, three-tier registry, federated mesh with signed mesh-index), web-native specs (OpenAPI 3 / AsyncAPI references, schema-driven request validation at the dispatch boundary), bidirectional channels (SSE one-way *and* WebSocket bidi with typed `ClientMessage` upstream), intent layer (intent → capability matching, auth-aware dispatch, streaming responses), and tier-used telemetry on every call.
- **Full Sigstore trust chain for manifest signing, including CRL-based certificate revocation.** Signature-vs-cert + cert validity window + Rekor inclusion proof + Fulcio cert chain + CRL revocation check against the signing cert's CDP extension. `TrustRoot::from_sigstore_tuf` auto-fetches trust material from Sigstore's Public Good TUF repository. `RevocationMode::BestEffort | Required | Skip` is caller-configurable.
- **MCP / A2A interop, both directions.** Ferridis-connected capabilities published as MCP tools by `ferridis-mcp-server` (works with Claude Code, Zed, Cursor, anything MCP-aware, and claude.ai via OAuth 2.1 + Dynamic Client Registration). MCP servers consumed as `Tier::Native` Ferridis capabilities by `ferridis-client::mcp` (stdio and SSE transports, session-loss auto-recovery). Live-validated against a real MCP SSE endpoint.
- **Local discovery broker + adapter self-registration.** `ferridis-discovery-broker` (port 7825) maintains a live service registry with SSE push. Adapters self-register on startup via `ferridis-adapter-sdk::broker` with a 240 s heartbeat RAII handle — no manual POST required.
- **stdio-to-SSE bridge.** `ferridis-stdio-bridge` wraps any stdio MCP server as an HTTP SSE endpoint the broker can register — solving the class of tools that have no network URL.
- **Six reference adapters.** Filesystem (`ferridis-adapter-fs`, path-traversal protection via `Root`/`RelPath` typestate), Claude Code CLI (stream-kind, drives `claude` over SSE), Google Calendar, Slack, GitHub, and Notion — each with 6 intents and wiremock-backed integration-test coverage.
- **Programmatic tool calling (v0.7, on `main`, unreleased).** `ferridis-ptc` offers Ferridis tools to Claude over the Messages API, with some tools callable only from code running in Claude's code-execution container, so intermediate results stay out of the model's context. Who may call each tool is set per tool, by the manifest's `callers` declaration or an `adapters.json` override, and enforced before dispatch in both directions; the caller is recorded on the tier-used log. Verified live on 2026-10-26: Claude's code read three files through the filesystem adapter and added them up, in five turns, for about $0.05.
- **Editor integrations.** VS Code (TypeScript extension registering `ferridis-mcp-server` with VS Code's native MCP runtime; Ferridis capabilities appear as Copilot Chat tools) and Zed (Rust → WASM extension registering `ferridis-mcp-server` as a context server).

The design process continues in public. If you build for AI, integrate AI, or use AI seriously enough to feel the friction — follow along, push back, file issues, propose changes. The protocol-level work tracker is [TODO.md](./TODO.md); the at-a-glance feature surface is [FEATURES.md](./FEATURES.md).

## Building from source

Full platform instructions — Linux x86_64, Linux ARM (native and cross-compiled), macOS (Intel and Apple Silicon), and Windows — are in [INSTALL.md](./INSTALL.md). The short version:

```bash
# Prerequisites: rustup (https://rustup.rs), libdbus-1-dev on Linux
git clone https://github.com/george-andrikopoulos/ferridis.git
cd ferridis/rust
cargo build --release --workspace
```

No OpenSSL dependency. The workspace uses `rustls` throughout. OS keychain integration is automatic — Linux Secret Service, macOS Keychain, or Windows Credential Manager, depending on platform.

## About the name

Ferridis is named for **Alexandros Ferridis**, a Greek high school teacher whose influence shaped the project's author. Full story: [about-the-name.md](./about-the-name.md).

## Contributing

This is an open design process. Issues, discussion threads, and pull requests welcome. The project's bias is toward changes that make the protocol simpler, not more capable. If a capability already exists in the web stack (OAuth, OpenAPI, AsyncAPI), Ferridis uses it rather than reinventing.

## License

**Apache License 2.0.** The Apache license includes an explicit patent grant — important for a protocol intended to be safely adoptable by enterprises.

## Maintainer

George Andrikopoulos — United Kingdom.

*[LinkedIn](https://www.linkedin.com/in/george-andrikopoulos/)*
