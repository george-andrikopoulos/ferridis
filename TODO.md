# Ferridis TODO

Actionable items in priority order. Cross off as you finish them. The detailed task list also lives in the project's task tool — this file is the human-readable mirror.

## Done

- [x] Manifesto drafted (the *why*)
- [x] Name locked in: **Ferridis** (tribute to Alexandros Ferridis)
- [x] Three architecture frictions resolved
- [x] Architecture document drafted (six layers, end-to-end)
- [x] Wire protocol sketched (handshake, manifest, calls, events)
- [x] README, CONTRIBUTING, LICENSE in place
- [x] Rust workspace scaffolded under `rust/`
- [x] `ferridis-core` types: typestate `Connection<S>`, parse-don't-validate `Manifest`, validated `IntentVerb` / `Tier` / `Tiers` / `AccessToken`
- [x] Toolchain pinned to Rust 1.95.0 (`rust/rust-toolchain.toml`)
- [x] Workspace bumped to Edition 2024, MSRV 1.95 (`rust/Cargo.toml`)
- [x] Pre-existing `secrecy 0.8` Serde-derive incompatibility fixed in `token.rs` (manual `Serialize`/`Deserialize` impls; redacted `Debug` preserved)
- [x] Root-level leftover workspace artifacts removed via `cleanup.sh`
- [x] **Task 7 — `ferridis-protocol`**: HTTP client, manifest fetcher, lazy schema fetcher, OAuth 2.0+PKCE (verifier/challenge/S256, authorization URL builder, token exchange), connection broker with `Connection<Pending>` → `Connection<Authorized>`, bearer-token call helper with Ferridis advisory headers. 14 tests pass (wiremock-based).
- [x] **Task 8 — `ferridis-adapter-sdk`**: `Capability` trait, axum-based `AdapterServer` (manifest + schema + dynamic `/intents/:verb` route, undeclared verbs → 404 by manifest check), `DispatchError`, `EventPublisher` trait with webhook implementation. 4 tests pass against a real HTTP server.
- [x] **Task 9 — `ferridis-adapter-fs`** (filesystem reference adapter): five intents (`read-file`/`write-file`/`list-dir`/`search-files`/`move-file`), path-traversal protection via `Root` + `RelPath` typestate (illegal-escape unrepresentable). 6 unit tests + 7 end-to-end integration tests against a real spun-up adapter pass.
- [x] **Trusted-reconstruction for `Connection<S>`** (prerequisite for Task 10): added `pub(crate)` `from_parts` ctors on every `Connection<S>` state and four projection / four builder methods on `StoredConnection` (`as_pending`/`as_authorized`/`as_expired`/`as_revoked` ←→ `from_pending`/`from_authorized`/`from_expired`/`from_revoked`). Reconstruction logic lives entirely inside `ferridis-core`; no new public surface on `Connection<S>` itself. 5 round-trip tests pass.
- [x] **`Capability` trait doc-comment** about v0.1 input-validation expectations (implementer is responsible; schema-driven validation in the SDK lands in v0.2).
- [x] **Task 10 — `ferridis-client`**: consumer-side library. Plaintext on-disk wallet with `0o600` perms on Unix (v0.2 keychain integration noted in doc), in-memory `Registry` with TTL, `Client` composing wallet + registry + HTTP for `register` / `candidates_for_intent` / `insert_connection` / `dispatch`. Honours manifest's `auth` declaration (`None` → `call_anonymous`, `Oauth2` → wallet-resolved `call`, `ApiKey` → `UnsupportedAuthMethod` for v0.1). Tier-used logging via `tracing`. 10 unit tests + 7 real-to-real integration tests against a real `ferridis-adapter-fs` over real HTTP (no wiremock).
- [x] **`ferridis-protocol` upstream change**: added `call_anonymous` for `auth: none` capabilities (used by the client's dispatch path). 15 tests pass on protocol crate.
- [x] **Task 11 — VS Code Ferridis integration**: new `ferridis-cli` sidecar binary (JSON-RPC 2.0 over stdio, wraps `ferridis-client`; 5 unit + 4 e2e tests pass) and new `editors/vscode/` TypeScript extension that spawns the sidecar, registers the five filesystem-adapter intents as VS Code Language Model Tools (Copilot-callable), and ships commands for manual adapter registration / listing / sidecar restart / log viewing. Auto-registers configured adapters from `ferridis.adapters` on activate. `ferridis.vsix` packaged at 11.84 KB. Requires GitHub Copilot Chat to be installed to demo.
- [x] **MCP-publisher shim — `ferridis-mcp-server`** (v0.2 work brought forward to v0.1, demo target after discovering host had Claude Code in VS Code rather than Copilot). New binary crate that exposes Ferridis-connected capabilities as an MCP server (stdio transport, MCP protocol `2024-11-05`). Implements `initialize`, `notifications/initialized`, `tools/list`, `tools/call`, `ping`. Maps each Ferridis intent to one MCP tool with a per-intent input schema. Adapter config shape shared with VS Code extension (`[{capability, manifestUrl, baseUrl}]`). 7 unit + 5 end-to-end tests pass; the e2e tests drive the binary via the MCP protocol from a test harness, exactly as a real MCP client would. **Demo verified end-to-end in Claude Code in VS Code: multiple MCP servers running side by side in one client session, Ferridis filesystem read working alongside existing MCP servers without touching them.** This is the v0.1 launch demo.

## Verify (passing on Rust 1.95.0 / Edition 2024 as of last run)

- [x] `cd rust && cargo check --workspace` — workspace compiles (14 crates: core, protocol, adapter-sdk, adapter-fs, adapter-claude-cli, adapter-gcal, adapter-slack, adapter-github, adapter-notion, client, cli, mcp-server, discovery-broker, stdio-bridge)
- [x] `cargo test --workspace` — full test suite passes across all 14 crates (see FEATURES.md for per-crate breakdown)
- [x] `cargo clippy --workspace --all-targets -- -D warnings` — zero warnings
- [x] `bash cleanup.sh` — root-level leftovers removed

## Implementation (in build order)

### Task 7 — `ferridis-protocol` (wire layer) — ✅ done

Sits between `ferridis-core` and the consumers.

- [x] Add `ferridis-protocol` crate to workspace
- [x] HTTP client built on `reqwest` + `tokio`
- [x] Manifest fetcher: `ferridis://` URL → `Manifest` (uses `ferridis-core`)
- [x] Schema fetcher: lazy-load schema bytes from manifest's `schema_url`
- [x] OAuth 2.0+PKCE flow (verifier/challenge S256, authorization URL builder, token exchange)
- [x] Connection broker: `start_connect` → `Pending`, `complete` → `Authorized`
- [x] Bearer-token call helper with `X-Ferridis-Connection` / `X-Ferridis-Intent` headers
- [x] Error mapping → `ProtocolError` (Transport / BadStatus / InvalidManifest / OAuth / ConnectionExpired / ConnectionRevoked / CapabilityNotFound / IntentNotSupported / TierUnavailable / ConsentRequired / RateLimited / Json / InvalidUrl / UnknownState)
- [x] 14 wiremock-backed tests

Reference: [wire-protocol.md](./wire-protocol.md).

### Task 8 — `ferridis-adapter-sdk` (publisher framework) — ✅ done

The framework adapter authors implement against.

- [x] Add `ferridis-adapter-sdk` crate to workspace
- [x] `Capability` trait: declare manifest, declare schema, dispatch calls
- [x] HTTP server (axum) — `GET /manifest.json`, `GET /schema`, `POST /intents/:verb` (with manifest-check 404 for undeclared verbs)
- [x] `DispatchError` enum with HTTP-status mapping
- [x] Event publishing helpers (`EventPublisher` trait + webhook implementation)
- [x] Schema-driven request validation — `validate_body` + `ValidationOutcome` + `Capability::body_schema` hook + server gate (landed v0.4)
- [ ] Derive macros for common patterns (deferred; the trait is small enough to implement by hand)

### Task 9 — Filesystem reference adapter — ✅ done

The first end-to-end Ferridis adapter, modeled on MCP's filesystem-server.

- [x] Add `ferridis-adapter-fs` crate
- [x] Implement `Capability` from the SDK
- [x] Intents: `read-file`, `write-file`, `list-dir`, `search-files`, `move-file`
- [x] Embedded manifest declaring those intents and an embedded OpenAPI schema
- [x] Path-traversal protection via `Root` + `RelPath` typestate (illegal-escape unrepresentable)
- [x] 6 unit tests on path traversal + 7 end-to-end integration tests against a real spun-up adapter

### Task 9b — Claude Code CLI adapter — ✅ done (2026-05-13)

Drives the `claude` CLI (Claude Code) as a stream-kind Ferridis capability — the stopgap for remote-controlling Claude Code while editor agent panels (Zed, Cursor) lack a public extension API for their assistant surface.

- [x] Add `ferridis-adapter-claude-cli` crate to workspace
- [x] Typestate-validated inputs: `Prompt` (non-empty, bounded), `AllowedCwd` against operator-supplied `AllowedRoots`, `Model` enum gated by `ModelAllowList`, UUID-validated `SessionId`
- [x] `submit-prompt` (stream-kind) — spawn `claude -p <prompt> --output-format stream-json --verbose [--session-id …] [--model …]`, line-stream stdout JSON
- [x] `resume-session` (stream-kind) — same but with `-r <session_id>` to continue
- [x] `kill_on_drop` on the child so a stuck `claude` can't outlive a dropped consumer stream; non-zero exits surface as an `adapter-event` chunk before the SDK's `end`
- [x] Example `serve` binary (`--bind`, `--allowed-cwd` repeatable, `--default-cwd`, `--default-model`, `--model-alias` repeatable, `--claude-binary`)
- [x] 7 input unit tests + 3 hermetic e2e tests against a stub `claude` script (round-trip, undeclared-intent 404, disallowed-cwd surfaces SSE `error` chunk) + 1 `#[ignore]`'d live test against real `claude`
- [x] Verified live: `submit-prompt(prompt="Reply with exactly the word PONG…", model=haiku)` → `result.result == "PONG"` over SSE with `session_id` captured; `resume-session(session_id=…, prompt="What word did I ask you to say?")` → same session_id, `result.result == "PONG"` (real continuation)
- [x] **Recursive case verified end-to-end through `ferridis-mcp-server`** (2026-05-13) — when adapters.json registers the running claude-cli adapter as a Native entry (`capability: "ferridis://personal.local/claude/cli@v1"`, `manifestUrl/baseUrl: http://127.0.0.1:7823/…`), the publisher advertises `ferridis_personal_claude_cli_v1_submit_prompt` + `_resume_session` on `tools/list` and `tools/call` returns the live Claude session's output. Required teaching the publisher to handle stream-kind intents: pre-fix, `tools/call` only routed through `Client::dispatch` (request-kind), so stream-kind intents surfaced as `isError: true`. The fix branches by `manifest.intent_kind()`: stream-kind goes through `Client::dispatch_streaming`, collects all chunks, extracts the final `result.result` text as the primary response, and bundles the full chunk array beside it (an MCP-2024-11-05 host sees a single response — progressive forwarding waits for MCP notifications-as-chunks to land). New regression test `tools_call_handles_stream_kind_intents` (in-test stream-kind Capability + AdapterServer + the real MCP shim binary) guards the path. Live PONG round-trip through the shim's stdout: 7 stream-json chunks collected, `result == "PONG"` extracted.

### Task 10 — `ferridis-client` (consumer side) — ✅ done

- [x] Add `ferridis-client` crate to workspace
- [x] Wallet (plaintext JSON, `0o600` perms on Unix, versioned format; v0.2 keychain integration documented)
- [x] Manifest registry with TTL freshness check
- [x] Personal-tier resolver (intent → candidates); three-tier traversal deferred to v0.2 alongside public-mesh registry
- [x] **Three-tier registry traversal** (2026-05-11, v0.2) — new `RegistryTier` enum (`Personal | Org | Public`). Each `CapabilityRecord` carries a tier (defaults to `Personal`); `CapabilityRecord::with_tier` + `Registry::set_tier(cap, tier)` are the supported retagging APIs. `candidates_for_intent` returns matches sorted Personal → Org → Public, so the host's own configuration always wins ties. New `candidates_for_intent_by_tier` returns matches grouped, for tier-aware UIs / per-tier policies (e.g., "prompt before invoking a public-tier capability"). `Public` is wired but inert until M4 (public mesh) and M3 (manifest signing) land — hosts can tag for tests but no records arrive through it yet. Publisher-side `--org-adapters-config` flag is the natural next step (the tier API is in place; the flag is documented as queued).
- [x] Dispatch honouring manifest `auth` (None / Oauth2 / ApiKey-as-unsupported)
- [x] Tier-used logging via `tracing::info!` (capability, intent, tier, auth, connection_id)
- [x] 7 real-to-real integration tests against `ferridis-adapter-fs` (no wiremock)
- [x] **Schema-driven request validation at dispatch boundary** (2026-05-11, v0.2) — `Client::dispatch` now pre-validates the outbound body against `CapabilityRecord.input_schemas[intent]` (populated by yesterday's M5 work for MCP-backed capabilities). New `validate_dispatch_body` helper uses `jsonschema 0.30` and renders error paths as `instance_path: detail`. New typed error: `ClientError::InvalidArgs { capability, intent, details }`. Lenient on malformed schemas: a schema that fails to compile logs a WARN and lets dispatch proceed (the adapter remains the final validator). Catches the exact bug that motivated yesterday's schema-preservation work: a host stringifying `limit: 5` would now be rejected at the client boundary instead of the stdio child. 5 new tests cover the boundary (valid args, wrong type, out-of-bounds, additionalProperties:false, broken-schema lenience). Capability-level OpenAPI splitting for native adapters is the remaining piece, queued separately.
- [x] **Event subscription management — SSE one-way (M6 minimal)** (2026-05-11, v0.2) — `ferridis-protocol::events` module ships the wire-level pieces: `ServerEvent { name, data, id }`, `subscribe(client, url)` opening an SSE connection and returning `impl Stream<Item = Result<ServerEvent, ProtocolError>>`, and `parse_sse_stream(bytes)` for pure-function testing. State-machine parser handles multi-line `data:`, chunk boundaries mid-event, `\r\n` line endings, missing `event:` defaulting to `"message"`, dropped no-data blocks, and ignored comments — 7 unit tests cover those cases. `Client::subscribe(capability, channel)` builds the URL by convention (`{base_url}/events/{channel}`) for Native-backed capabilities; MCP-backed returns `UnsupportedAuthMethod` for v0.2. End-to-end test: real axum SSE server emits 3 events; the client receives them in order with names, JSON payloads, and `id` round-tripped. **v0.3 — manifest-declared event channels shipped** (2026-05-12) — `ferridis_core::Manifest` gained `event_channels: Vec<EventChannel>` (`name` + optional `chunk_schema_url`); parses are validated for syntactically-correct names (same `[a-z0-9-]+` class as `IntentVerb`) and rejects duplicates. `Manifest::has_event_channel(name)` is the lookup. `Client::subscribe` enforces declared channels when the manifest carries any (legacy fall-through stays for capabilities with empty `event_channels`); undeclared channels surface a typed `ClientError::EventChannelNotDeclared { capability, channel, declared }`.

**v0.3 — dispatch_streaming shipped** (2026-05-12) — `Client::dispatch_streaming(capability, intent, body)` returns `impl Stream<Item = Result<Value, ClientError>>` for stream-kind intents. Wire transport: POST to `{endpoint}/intents/{verb}` with `Accept: text/event-stream`; adapter emits `event: chunk` lines (yielded as `Ok(Value)`), `event: end` (terminates), `event: error` (yields one typed error then terminates). Auth-aware (anonymous + OAuth bearer). Client-boundary kind checks: `Client::dispatch` on a stream-kind intent returns typed `IntentRequiresStreaming`; `Client::dispatch_streaming` on a request-kind intent returns typed `IntentNotStreaming`.

**v0.3 — adapter-sdk SSE-emit helper shipped** (2026-05-13) — `Capability` trait gained `dispatch_stream(intent, body) -> Result<IntentStream, DispatchError>` with a default impl that returns `UnsupportedIntent` so existing adapters keep compiling. `IntentStream` is a typed `Pin<Box<dyn Stream<Item = StreamItem> + Send>>`. `AdapterServer::dispatch_intent` now routes by manifest-declared `IntentKind`: Request → existing JSON path, Stream → `dispatch_stream` wrapped in axum's SSE response with `chunk` / `error` / `end` events. `serve_manifest` re-serializes intent metadata in the structured form so an intent's kind survives the SDK→client round-trip. End-to-end test in real_to_real.rs: adapter implements `dispatch_stream` with three chunks; `Client::dispatch_streaming` receives them in order; `event: end` cleanly terminates the consumer stream. MCP-backed streaming dispatch still queued for v0.4.

**v0.3 — WebSocket bidi transport shipped** (2026-05-13) — `ferridis-protocol::ws` module ships the wire primitive: `connect_ws(url, bearer)` returns a `WsConnection` that splits into `(WsSender, WsReceiver)` for independent send/receive halves. `ClientMessage` is a typed enum (`Subscribe { channel } | Unsubscribe { channel } | Ack { id } | Control { payload }`) — illegal upstream shapes unrepresentable. `ServerEvent` is reused from the SSE module, so consumer code is transport-agnostic. `Authorization: Bearer` header propagates on the upgrade. Built on `tokio-tungstenite 0.29` (rustls-webpki-roots). Two end-to-end tests against a real axum WebSocket server: full bidi round-trip + split-halves concurrent usage. New typed error: `ProtocolError::WebSocket { url, detail }`.

**v0.5 — Automatic service discovery + broker persistence shipped** (2026-05-23) — Two discovery mechanisms, both TDD: (1) **mDNS scanning** — `MdnsScanner` in `ferridis-protocol` browses `_mcp._tcp.local` and `_ferridis._tcp.local` via `mdns-sd 0.13`, delivers `DiscoveredService` over a tokio `mpsc` channel; `Client::discover_mdns()` auto-registers every resolved MCP service via `register_mcp_sse`, returns a drop-to-stop `DiscoveryHandle`. (2) **Push-from-broker** — new `ferridis-discovery-broker` binary crate (axum, port 7825): `POST /discovery/register` stores services with 5-min TTL, `GET /discovery/services` returns live entries, `GET /discovery/events` pushes new arrivals over SSE; `Client::discover_broker(url)` fetches the snapshot then subscribes to the stream, auto-registering MCP services in the background. Systemd user unit ships. (3) **Persistent registrations** — `DELETE /discovery/services/:name` (204/404), `"persistent": bool` on list responses, `Lifetime` enum (`Ephemeral { expires_at }` | `Pinned`, TDP Pattern 5) replacing boolean; pinned entries survive TTL and restarts; state file stores only pinned entries; `load_state()` always sets `Lifetime::Pinned`. Eight new TDD tests. Full workspace: 0 failures, clippy clean.

- [x] **Adapter self-registration at startup (v0.6)** — `ferridis-adapter-sdk::broker` ships `BrokerUrl`, `ServiceName`, `AdapterUrl`, `RegistrationPersistence` (Ephemeral/Pinned), `ServiceKind` (Mcp/Ferridis), `BrokerConfig`, `BrokerRegistration` RAII handle (heartbeat at 240 s, abort-on-drop). `ferridis-adapter-fs` and `ferridis-adapter-claude-cli` `serve` examples wired with `--discovery-broker`, `--service-name`, `--pinned`.
- [x] **`ferridis-stdio-bridge` (v0.6)** — new binary crate. Bridges any stdio MCP server to HTTP SSE (session-per-connection model, `start_kill` on drop). `--command`, repeatable `--arg`, `--env KEY=VALUE`, `--bind` (default 7826), `--discovery-broker`, `--service-name`, `--pinned`. Registers as `ServiceKind::Mcp` with the broker. TDP: `Command`, `SessionId`, `SpawnConfig::spawn() -> SpawnedChild`, `BridgeError`.

**v0.4 — Protocol-layer follow-ups shipped** (2026-05-22) — Seven tasks, all TDD (RED→GREEN→commit): (1) `ChannelTransport { Sse | WebSocket }` enum in `ferridis-core` with `EventChannel` field privacy enforced and `Manifest::channel_transport` lookup; (2) Adapter-SDK request validation via `validate_body` + `ValidationOutcome` + `Capability::body_schema` hook + server gate; (3) `Client::subscribe_ws` with `ClientError::WrongTransport`; (4) Adapter-SDK `WsHandler` trait with `WsConnId` + `WsMessage`; (5) `ReconnectCursor` + `subscribe_with_cursor` for SSE `Last-Event-ID` reconnect; (6) `ConfidenceScore` + `ScoredCandidate` + `Registry::candidates_for_intent_scored` (Personal=100, Org=66, Public=33, lexicographic tie-breaking); (7) `BackpressureSignal { Continue | SlowDown | Halt }` + `StreamChunk`. Full workspace: 0 failures.
- [x] **OS keychain integration via `keyring` crate** (2026-05-11, v0.2) — `WalletStore` trait (in `wallet_store.rs`) with two impls: `KeychainStore` (production, talks to Linux Secret Service via D-Bus / macOS Keychain / Windows Credential Manager through `keyring` 3.6) and `MemoryStore` (tests / embedded). Per-connection entries (account = capability ref) plus one well-known `__ferridis_wallet_index__` entry listing keys, since `keyring` has no enumerate API. **Refuses to run** if the OS keychain backend is unreachable — no plaintext fallback (that would be a downgrade vector). Legacy v0.1 plaintext wallets at `$HOME/.ferridis/wallet.json` are also refused; users must migrate manually. New typed errors: `WalletBackendUnavailable`, `WalletBackendError`, `LegacyPlaintextWalletDetected`. `Wallet` API changed: `open_keychain(namespace)` / `ephemeral()` / `with_store(store)` replace the old `open(path)`. Each insert/remove is atomic at the store level — `save()` is gone. Binaries (`ferridis-cli`, `ferridis-mcp-server`) take `--keychain-namespace` or `FERRIDIS_KEYCHAIN_NAMESPACE`. Tests opt into in-memory via `FERRIDIS_WALLET_MEMORY=1` (explicit escape hatch, never a silent fallback).

### `ferridis-protocol` hardening — ✅ connection-pool fix landed

- [x] `pool_idle_timeout(15s)`, `pool_max_idle_per_host(2)`, `connect_timeout(5s)` on the shared HTTP client to prevent the wedge symptom we hit in Task 11 demo testing (stale localhost keepalive surviving past adapter restart). Public constants exposed; doc-comment explains the rationale.

### Task 11 — VS Code Ferridis integration — ✅ done

- [x] `ferridis-cli` sidecar binary — JSON-RPC 2.0 over stdio, wraps `ferridis-client`. Methods: `register` / `candidates_for_intent` / `dispatch` / `insert_connection` / `list_capabilities`. 5 unit + 4 end-to-end tests.
- [x] TypeScript extension scaffold under `editors/vscode/` (esbuild bundled, vsce-packaged)
- [x] Sidecar binary protocol: VS Code TS ↔ Rust `ferridis-cli` over stdio (correlated request/response by JSON-RPC id)
- [x] Adapter configuration in VS Code settings UI (`ferridis.cliPath`, `ferridis.walletPath`, `ferridis.adapters`)
- [x] Bridge VS Code's Language Model tool surface to the five filesystem-adapter intents (Copilot-callable, statically declared in `package.json#contributes.languageModelTools`)
- [x] Commands: `Ferridis: Register Adapter` / `List Registered Capabilities` / `Restart Sidecar` / `Show Logs`
- [x] Co-exist with any pre-existing MCP integration (the extension does not touch VS Code's MCP plumbing — both surfaces are usable in one Copilot session)
- [x] First demo: ask Copilot in VS Code to read a file via Ferridis filesystem adapter

**Demo path**:
1. `cargo build --release -p ferridis-cli` — binary at `rust/target/release/ferridis-cli`.
2. `cargo run --release -p ferridis-adapter-fs --example serve-fs -- --root /tmp/ferridis-demo --bind 127.0.0.1:7821` — spins the adapter.
3. `code --install-extension editors/vscode/ferridis.vsix`
4. In VS Code Settings (JSON), set `ferridis.cliPath` to the absolute path of the release binary, and add the adapter to `ferridis.adapters`:
   ```jsonc
   {
     "ferridis.cliPath": "/absolute/path/to/rust/target/release/ferridis-cli",
     "ferridis.adapters": [
       {
         "capability": "ferridis://public.ferridis.io/ferridis/fs@v1",
         "manifestUrl": "http://127.0.0.1:7821/manifest.json",
         "baseUrl": "http://127.0.0.1:7821/"
       }
     ]
   }
   ```
5. Reload window; open Copilot Chat and ask *"read the file hello.txt from the Ferridis filesystem"* — Copilot calls `ferridis_fs_read_file` → extension → sidecar → adapter.
6. Same session: ask *"turn on the living-room light"* — routes through your existing HA MCP plumbing, untouched. Co-existence verified.

### Task 12 — Zed Ferridis integration — ✅ done (context-server path)

- [x] Native Rust Zed extension under `editors/zed/`, pure Rust, compiled to `wasm32-wasip1` (no JS, no manual stdio in the extension)
- [x] Registers `ferridis-mcp-server` as a Zed context server through Zed's extension API
- [x] User configures `binary_path` and `adapters_config` in Zed settings under `context_servers.ferridis-mcp.settings`
- [x] Optional `wallet_path` override
- [x] Co-exists with any pre-existing MCP integration in Zed (same principle as Claude Code in VS Code)
- [ ] **Future: pure-WASM Ferridis client.** A future extension version uses Zed's `HttpClient` API to talk Ferridis directly to adapters with no subprocess at all. Requires WASM-compatible reimplementation of the client's HTTP layer because `reqwest`/`tokio` don't compile to `wasm32-wasip1` out of the box.

## Queued (not yet scheduled)

- [x] **MCP / A2A interoperability shim, half 1: Ferridis → MCP** — `ferridis-mcp-server` ships in v0.1 (pulled forward from v0.2 plan). Any MCP-aware client (Claude Code, Zed, Cursor, MCP-supporting Copilot) consumes Ferridis-connected capabilities as MCP tools.
- [x] **MCP / A2A interoperability shim, half 2: MCP → Ferridis** — `ferridis-client` consumes MCP servers as `Tier::Native` capabilities. New module `ferridis-client/src/mcp/` with `McpClient`, `StdioTransport`, `SseTransport`. `Client::register_mcp_stdio` + `register_mcp_sse` open the MCP connection, perform handshake, project `tools/list` into a synthetic Ferridis manifest, route subsequent `Client::dispatch` calls through the live MCP connection. Roundtrip self-test passes (ferridis-client ↔ ferridis-mcp-server over stdio). **Live demo against a real MCP SSE endpoint verified — a Home Assistant sensor reading was returned, routed entirely through Ferridis.** Both halves of the interop shim now ship.
- [x] **MCP-consumer SSE session-loss auto-recovery** (2026-05-12) — Discovered when an HA MCP server restarted and the long-running `ferridis-mcp-server` children kept POSTing to dead `sessionId`s, getting `404 Not Found` with no recovery path. Fix: SSE transport now detects 404 on POST and surfaces a typed `ClientError::McpSessionExpired`. `McpTransport` trait gained a `reconnect()` method (no-op default for stdio, real re-handshake for SSE). `McpClient::call_tool` catches the typed error, calls `transport.reconnect()`, replays `initialize` + `notifications/initialized` on the fresh session, retries the original `tools/call` exactly once. The reader-task spawn was extracted into a free helper so both `open` and `reconnect` use the same code path; `post_url` became `Arc<Mutex<Option<Url>>>` so the spawned task can install the new endpoint without holding `Arc<Self>`. End-to-end test against an axum mock SSE server with a session counter that the test forces forward to simulate the VM-restart situation precisely — the first request succeeds on session 1, the test moves the server's counter to 99, the next request gets `McpSessionExpired`, `reconnect()` bumps to session 100, the retry succeeds.
- [ ] **AAIF engagement** — open issues / SEPs against MCP for ideas Ferridis encodes that could land in the wire standard (manifest-vs-schema split, three-tier discovery, intent vocabulary governance). Track which Ferridis opinions are worth upstreaming vs. keeping as Ferridis-layer opinions.
- [x] **Preserve MCP `inputSchema` verbatim in synthetic Ferridis manifests** (2026-05-11) — when `project_tools_to_manifest` projects an MCP `tools/list` result, the per-tool `inputSchema` now flows through `ProjectedManifest::intent_to_input_schema` → `CapabilityRecord::input_schemas` (Arc-shared) → `AdapterEntry::input_schemas` → publisher `tools/list`. Three-tier precedence in `ToolCatalogue::from_adapters`: threaded MCP schema > hardcoded fs table > permissive `additionalProperties: true` fallback. Verified end-to-end against real `linux-health-mcp`: `journal_query` now republished with `limit: integer 1..10000`, typed `priority`/`unit`/`since`, `additionalProperties: false`. Integer `limit:5` round-trips cleanly (was rejected as string `"5"` before). 4 new tests guard the regression: schema-verbatim through projection, catalogue prefers threaded over hardcoded, hardcoded still wins when no schema is threaded, permissive fallback for unknown MCP servers.
- [x] **Publisher no longer all-or-nothing on adapter registration failure** (2026-05-11) — `register_adapters` was refactored from `Result<Vec<AdapterEntry>, String>` to `(Vec<AdapterEntry>, Vec<AdapterRegistrationFailure>)`: each adapter is processed independently and failures collect into structured records (`kind: Native|McpSse|McpStdio`, `descriptor`, `error`). Each adapter is bounded by a 10s registration timeout (`ADAPTER_REGISTRATION_TIMEOUT`) so an unreachable host on a routable-but-dead subnet can't stall startup past Linux's default TCP connect window. `main()` logs every failure at ERROR with structured fields, emits a WARN summary if any failed, and continues. Open question resolved: `tools/list` advertises the working subset only — no error placeholders, since a tool guaranteed to fail on every call is a worse UX than a tool that simply isn't there. Verified end-to-end with a 4-entry config (2 bad / 2 good): EXIT 0 in exactly the 10s timeout window, 10 working tools surfaced. 2 new e2e tests guard the regression (`partial_failure_keeps_successful_adapters_alive`, `partial_failure_with_all_adapters_failing_still_serves`).
- [x] **Manifest signing scheme — full Sigstore trust chain** (2026-05-11/12): **Sigstore (cosign + Fulcio + Rekor)** decision locked. All three trust-chain checks shipping: sig-vs-cert + cert validity window + Rekor inclusion proof + **Fulcio cert chain validation**. Wire format is the cosign-bundle JSON; `ferridis-protocol::signing` ships `CosignBundle`, `TrustRoot`, `VerifiedManifest`, `SigningError`, three verify entry points (`verify_signed_manifest`, `verify_signed_manifest_at(now)`, `verify_signed_manifest_with_trust_root(bundle, trust_root, now)`). Real ECDSA P-256 signature-vs-cert verification via `sigstore 0.13`, X.509 `notBefore`/`notAfter` window enforcement via `x509-cert`, Rekor inclusion proof via `SignedArtifactBundle::new_verified`, and Fulcio chain validation via `rustls-webpki`'s `EndEntityCert::verify_for_usage(KeyUsage::client_auth())` against trust anchors built from `TrustRoot.fulcio_cert_der`. `extract_verification_key` accepts both real X.509 cert PEMs (extracts SPKI) and bare SPKI PEMs (test fixtures). 10 tests including two real-CA chain round-trips with `rcgen`-generated test chains (success path + wrong-CA rejection as `FulcioChainInvalid`). **v0.3 — TUF-based trust root auto-fetch shipped** (2026-05-13) — `TrustRoot::from_sigstore_tuf(cache_dir)` resolves Fulcio CAs + Rekor public keys from Sigstore's Public Good TUF repository (`tuf-repo-cdn.sigstore.dev`) via the `sigstore-trust-root` feature on the `sigstore` crate. Fulcio certs install as DER trust anchors; Rekor SPKI DER is base64-wrapped into `BEGIN PUBLIC KEY` PEM so it round-trips through the existing PEM-based [`with_rekor_key`] API. New `SigningError::TufFetch(String)` variant. **Two-scheme bug found and fixed in the same change**: the 2026 production TUF root carries both an ECDSA P-256 *and* an Ed25519 Rekor key in one response. `rekor_verification_keys()` switched from `from_der(default)` (ECDSA-locked, silently dropped the Ed25519 key in sigstore-rs's own client_builder) to `try_from_der` (auto-detect scheme from SPKI algorithm OID) so both schemes survive. Live `#[ignore]`d test against the real Sigstore Public Good Instance asserts at least two distinct schemes parse cleanly + every key parses via the production path. **Shipped (2026-05-24)**: cert revocation — `RevocationMode` (BestEffort/Required/Skip), `CrlData`, `extract_cdp_url`, `fetch_crl`, `check_cert_not_revoked`, `verify_signed_manifest_with_revocation`. 8 new tests pass.
- [~] **Intent vocabulary v0 — drafted** (2026-05-11) — 35 verbs across 8 categories (messaging, calendar, files, search, payments, identity, scheduling, common), drafted in [`vocabulary.md`](./vocabulary.md). **Governance: proposed for AAIF.** Until AAIF accepts (or refers elsewhere), maintained in this repo by George with PRs welcome. The doc carries naming convention, versioning policy (new verbs only, never re-defining existing ones), contribution criteria (two-publisher rule, named input shape, rationale), and explicit out-of-scope. Open questions captured (search scope, streaming, locale, audit, cancellation). Submission-to-AAIF folded into the AAIF-engagement TODO item.
- [~] **Public mesh registry — phase 1 + 2 shipped** (2026-05-11) — `ferridis-protocol::mesh` ships `MeshClient` with `fetch(capability_ref)`, `fetch_and_verify(capability_ref)`, and a per-capability TTL cache (`DEFAULT_MESH_CACHE_TTL = 300s`, overridable via `with_cache_ttl`). URL convention: `{mesh_root}/{registry}/{namespace}/{id}/{version}/manifest.json` plus companion `.cosign.bundle`. `Client::register_from_mesh(capability, mesh_root, fallback_base_url)` prefers the manifest's new `endpoint_url` field over the caller-supplied fallback; registers with `RegistryTier::Public`. `ferridis_core::Manifest` gained an optional `endpoint_url` field for capabilities published to a mesh (caller-supplied base URL becomes a transitional fallback; v0.3 removes the fallback). End-to-end test: real axum mock mesh + real in-test ECDSA P-256 signing key + verify + Public-tier assertion. **v0.3 federation + signed mesh-index shipped** (2026-05-12) — `MeshIndex` typed payload (`mesh_id`, `issued_at`, `expires_at`, entries of `(capability, intents, category)`), `MeshIndexEntry`, `MeshClient::fetch_index_and_verify(&TrustRoot)` (loads `{mesh_root}/index.json` + cosign bundle, verifies through the full Sigstore trust chain via `verify_signed_manifest_with_trust_root`, rejects expired indexes), and `FederatedMesh` (priority-ordered chain of mesh clients; `fetch_and_verify` falls through on 404, returns `CapabilityNotFound` when no tier has the cap). End-to-end tests: federated fall-through (mesh A 404s, mesh B succeeds) + federated `CapabilityNotFound` when no tier has the cap + 5 mesh-index unit tests (intent lookup, category lookup, expiry, JSON round-trip, index URL convention). **All queued items shipped** (2026-05-24): `Client::register_from_federated_mesh` (client.rs:688) + `Client::register_all_from_mesh` (client.rs:745) fully wired; mesh-index-driven Public-tier auto-population complete.
- [x] **Tighten `serde(transparent)` newtypes to `try_from = "String"` / `into = "String"`** (2026-05-11, v0.2) — `IntentVerb`, `Category`, `Summary`, and `CapabilityVersion` all routed their `Deserialize` through `transparent`, which silently bypassed their validating constructors. Now each goes through `TryFrom<String>` (calls the existing `::parse`) so malformed JSON like `"Send_Message"` fails to deserialize where it used to slip through. Matches the pattern already used by `CapabilityRef`. +4 regression tests prove the new boundary holds.
- [x] Reference adapters: **GitHub done** (`ferridis-adapter-github`) — 6 intents, wiremock e2e tests, broker self-registration, `--api-base-url` for GitHub Enterprise.
- [x] Reference adapters: **Google Calendar done** (`ferridis-adapter-google-calendar`) — 6 intents (list-calendars, list-events, get-event, create-event, update-event, delete-event), OAuth 2.0 access token + optional auto-refresh (refresh token + client credentials), wiremock e2e tests, default port 7828.
- [x] Reference adapters: **Slack done** (`ferridis-adapter-slack`) — 6 intents (list-channels, post-message, get-messages, send-dm, get-channel-info, list-users), long-lived bot token (xoxb-) held server-side, `send-dm` two-step via `conversations.open` + `chat.postMessage`, Slack's always-200 error model handled in client layer. 2 unit + 9 e2e tests via wiremock. Default port 7829.
- [x] Reference adapters: **Notion done** (`ferridis-adapter-notion`) — 6 intents (list-databases, query-database, get-page, create-page, update-page, search), integration token held server-side, mandatory `Notion-Version: 2022-06-28` header, `query-database` and `update-page` strip routing fields before forwarding to API. 4 unit + 9 e2e tests via wiremock. Default port 7830.
- [~] **Streaming response schema language — design extension + manifest plumbing shipped** (2026-05-11/12) — design lives in [`wire-protocol.md`](./wire-protocol.md). Capabilities declare per-intent `kind: "request" | "stream"` and (for streamed kinds) a `chunk_schema_url`. Wire transport reuses the SSE primitive from M6. Client API: `dispatch_streaming(capability, intent, body) -> impl Stream<Item = Result<Value, ClientError>>`. **v0.3 manifest plumbing shipped** (2026-05-12): `ferridis_core::IntentKind` (Request/Stream), `ferridis_core::IntentMetadata` (kind + optional chunk_schema_url), `Manifest::intent_kind(verb)` + `Manifest::intent_metadata(verb)`. Manifest's `intents` field accepts both flat strings (default kind: Request) and structured `{verb, kind, chunk_schema_url}` objects via serde-untagged. Validation enforces that streamed intents must declare a chunk_schema_url — silent default would be a footgun for clients trying to validate received chunks. **Shipped (v0.3, 2026-05-12)**: `Client::dispatch_streaming` fully implemented — see v0.3 notes above.

## Programmatic tool calling (PTC) — v0.7

Design gate: decisions PTC-1, PTC-2 and PTC-3 in the [architecture.md](./architecture.md) decisions log, all approved 2026-10-25.

- [x] **Step 1 — design gate.** Caller policy is `CallerPolicy { Direct | Code }`. Precedence, highest first: computed constraint > adapters.json override > manifest declaration > default `Direct`. Enforced both ways at dispatch.
- [x] **Step 2 — response schemas in `describe()`.** Pinned through the production path by mcp-server `tools_list_descriptions_carry_the_served_response_schemas`.
  - [x] `ferridis-protocol::openapi`: `ResponseSchemas` parsed from an OpenAPI 3 JSON document
  - [x] `ferridis-adapter-sdk::check_response`: conformance helper for adapter tests
  - [x] Convert the six reference adapters' embedded OpenAPI from YAML to JSON, with a response schema for every request-kind intent; add conformance assertions to each adapter's e2e suite
  - [x] `ferridis-client`: `CapabilityRecord` response schemas, `Client::load_response_schemas` (adapter `/schema` first, then `schema_url`)
  - [x] `ferridis-client::catalogue`: tool naming, `describe()`, input-schema precedence and stream collection, moved out of `ferridis-mcp-server` (PTC-3)
  - [x] `ferridis-mcp-server`: `tools.rs` becomes a thin projection; registration loads response schemas; e2e pin that `tools/list` descriptions carry the schema
- [ ] **Step 3 — `ferridis-ptc` crate** (lib + example): Messages API over reqwest, `PendingProgrammaticCalls` typestate, `ContainerId`/`ContainerExpiry` newtypes, caller-policy enforcement, recursive-`$ref` downgrade, `strict` stripped, caller recorded in tier-used telemetry. Also: move `AdapterConfig` + `register_adapters` into `ferridis-client` (PTC-3); add the manifest `callers` field (v0.7) and the adapters.json override (PTC-1).
- [ ] **Step 4 — tests**: wiremock replay of recorded API responses (no tokens spent by default); property tests (one result per programmatic call; a continuation holds only tool_results; a refusal never reaches the adapter); one `#[ignore]`'d live smoke test behind an env var, with its cost stated.
- [ ] README "What runs today": update only after the end-to-end live run works.

Out of scope for PTC (separate items, not started):

- [ ] Move `ferridis-mcp-server` from MCP 2024-11-05 to a current spec (`outputSchema` / `structuredContent`).
- [ ] Any `ferridis-mcp-server` change beyond what PTC-3 approved (the catalogue move and loading response schemas at registration).
- [ ] Derive native adapters' **input** schemas from the OpenAPI `requestBody` and retire the hardcoded fs table in the catalogue. The data is now parseable JSON, but this changes client-side dispatch validation, so it needs its own change.

## Enforcement gaps (from the FEATURES.md ledger audit, 2026-07-20)

Every **NOTHING YET — exposed** entry in [FEATURES.md](./FEATURES.md) is a documented behavior with no type or test holding it. Close each gap and update the corresponding ledger row in the same change:

- [x] `ferridis-stdio-bridge` test suite (2026-07-20) — crate split into lib+bin; 4 unit tests (`types.rs`) + 7 hermetic e2e (`tests/hermetic_bridge.rs`) against a rustc-compiled stub child: handshake, round-trip, arg/env plumbing, 404/400, child-exit session cleanup, spawn-failure 500
- [x] `EventPublisher` / webhook delivery tests (2026-07-20) — `ferridis-adapter-sdk/tests/webhook_delivery.rs`: real receiving server, BadStatus(500), Transport failure
- [x] Token-redaction regression tests (2026-07-20) — `ferridis-core/src/token.rs`: Debug never leaks for both tokens; serde round-trip preserves secret while Debug stays redacted
- [x] Client-side `EventChannelNotDeclared` rejection tests (2026-07-20) — `ferridis-client/tests/subscribe_channel_declaration.rs`, covers `subscribe` and `subscribe_ws`
- [x] SSE reconnect resume-behavior e2e (2026-07-20) — `ferridis-protocol/tests/sse_resume.rs`: drop mid-feed, reconnect with cursor, no replay / no gap
- [x] Broker self-registration heartbeat e2e (2026-07-20) — `BrokerConfig::with_heartbeat_interval` added (default 240 s unchanged); `ferridis-adapter-sdk/tests/broker_heartbeat.rs`: re-register within interval, abort-on-drop stops, pinned never heartbeats
- [x] Tier-used logging capture test (2026-07-20) — `ferridis-client/tests/tier_logging.rs` captures real tracing output through a real dispatch and asserts capability/intent/tier fields
- [x] HTTP pool-hardening — **accepted as config-constant-only** (2026-07-20): reqwest exposes no pool-state readback; constants public with rationale doc-comments
- [ ] Editor-extension automated tests (VS Code TS extension, Zed WASM extension) — currently manual verification only
- [x] Property-based tests (2026-07-20) — `proptest` added; `ferridis-core/tests/prop_parsers.rs` (8 properties: IntentVerb, CapabilityVersion, CapabilityRef) + `ferridis-protocol/tests/prop_sse.rs` (3 properties: chunk-boundary invariance, field fidelity, CRLF≡LF). A whole-`Manifest`-document generator remains a candidate follow-up.
- [x] **Backpressure wired end-to-end + cancellation propagation (v0.7, 2026-07-20)** — `Capability::dispatch_stream_flow` (default adapts `dispatch_stream`, existing adapters untouched); SDK emits `event: backpressure {"signal": …}` on state change, `Halt` terminates the stream; client surfaces halt as typed `ClientError::StreamHalted`, slow-down via tracing; wire is additive (pre-v0.7 clients ignore the event per SSE convention). Cancellation: consumer drop provably drops the adapter's intent stream (`consumer_drop_reaches_the_adapter_stream_drop`). Tests: client `backpressure_flow.rs`, sdk `stream_cancellation.rs` (wire-shape pin included).
- [x] `mcp_consumer` interop self-test made platform-correct (2026-07-20) — binary located beside the test executable (honors `CARGO_TARGET_DIR`, platform exe suffix); previously hardcoded a Unix `target/release` path and failed on Windows

## Open design questions

These do not block tasks 7–12 but should be resolved before v1.0:

- [ ] Who governs the controlled intent vocabulary? Foundation? Working group?
- [x] How are public-mesh manifests signed? Sigstore (cosign-bundle JSON) — shipped in v0.3/v0.4.
- [ ] How do cross-capability transactions work?
- [ ] What's the trust model for locally-running capabilities?
