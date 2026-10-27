# Ferridis architecture

*An opinionated, type-driven design layer above the wire protocols that connect AI to the world the user already lives in.*

## Executive summary

Ferridis is a **connection-and-mesh** design for the experience layer above the MCP/A2A wire standard. Users authorize *connections* — durable, consent-based links to services — instead of installing and running server processes. Capabilities are discovered through a **federated mesh** with lazy-loaded schemas. Events flow back from the world over **bidirectional channels**. An **intent layer** routes high-level goals to the right capability. The **browser** stays as a universal fallback for anything not yet wired up.

The user sees one thing: *I connected my calendar. The assistant uses it.*

## Relationship to MCP, A2A, and AAIF

The wire-level standard for connecting AI to tools is now governed under the **Agentic AI Foundation** (AAIF), a Linux Foundation directed fund co-founded by Anthropic, Block, and OpenAI in December 2025. **MCP** is the standard for agent-to-tool calls, with active SEP-based evolution and 170+ member organizations. **A2A** is Google's open standard for agent-to-agent communication — a complementary protocol in the same ecosystem, not an AAIF project.

Ferridis is not a wire-protocol proposal. The standards above already cover request/response framing, capability invocation, and basic discovery. What those standards deliberately leave to implementers is everything *around* the wire: how connections are authorized and stored, how the user discovers what's available without their model's context being flooded with schemas, how high-level intents are routed to specific capabilities, and what happens when the integration target has no API at all.

Those are the layers Ferridis takes opinions on. Concretely:

- **Connections, not servers**, as the user-facing metaphor — borrowed from how phone apps work.
- **Two-stage advertisement** — small always-loaded manifest, full schema on demand — so the model's context cost stays bounded as the connected world grows.
- **Three-tier federated registry** (personal → org → public) with explicit precedence and conflict surfacing.
- **A controlled intent vocabulary** so high-level goals route to the right capability without the model memorising 500 tool descriptions.
- **Browser/computer-use as tier 3** of the same protocol, not a separate product.
- **A type-driven Rust reference** with typestate and illegal-states-unrepresentable, complementing the existing TypeScript and Python MCP references.

MCP/A2A interoperability ships in both directions in the reference implementation: `ferridis-mcp-server` publishes Ferridis-connected capabilities as MCP tools for any MCP-aware client (Claude Code, Zed, Cursor, MCP-supporting Copilot), and `ferridis-client::mcp` consumes MCP servers as `Tier::Native` Ferridis capabilities over both stdio and SSE.

## Design principles

Five principles, in priority order. When two conflict, higher beats lower.

1. **Hide the wires.** The user never edits a config file, runs a server, or debugs a JSON-RPC error. Configuration is OAuth-grade simple.
2. **Be web-native.** Reuse OAuth, OpenAPI, AsyncAPI, JSON Schema, HTTP. Invent nothing the web has already standardized.
3. **Lazy by default.** Nothing loads into the model's context until it's needed. The default state of every capability is "discoverable but absent."
4. **Composable and federated.** No central authority owns the namespace. Public, organizational, and personal capabilities coexist.
5. **Attribution is structural.** Every capability declares its author, license, and provenance. The protocol carries this through to the user.

## The six layers

Ferridis is a stack. Each layer has a clear purpose and a clear interface to the layers above and below.

![Six-layer architecture](./diagrams/layers.svg)

### Layer 1 — Connections

A **connection** is a durable, consent-based link between a Ferridis runtime (e.g., an AI assistant) and a service (e.g., Google Calendar).

**What it carries:** the identity of the user, the scope of access, refresh credentials, an audit trail, and lifecycle metadata (created, last used, expiry).

**How it's established:** standard OAuth 2.1 (or OAuth 2.0 with PKCE). The runtime opens a browser window, the user grants the requested scopes on the service's own consent screen, and the resulting tokens land in the user's **connection wallet** — a local, encrypted store on the user's device.

**Consent model:** every connection lists exactly the intents and capability scopes it grants. Granular revocation is first-class: users can revoke a single connection without affecting others.

**Why this matters.** The install-a-server-per-tool experience leaves credential storage and scope to each server, which means every adapter reinvents identity. Ferridis takes the opposite position: identity is a solved problem at the web layer, use OAuth, and bind the result to a durable user-owned connection.

### Layer 2 — Capability mesh

A **capability** is something the world can do for the AI: read a calendar, send a message, query a database, render a document. The **mesh** is how capabilities are discovered.

**Two-stage advertisement:**

- **Manifest** (always loaded): ≈200-token summary with ID, category, intent verbs, one-line description, schema URL, supported execution tiers.
- **Schema** (loaded on demand): full OpenAPI 3 or AsyncAPI spec.

**Three-tier registry:**

| Tier | Scope | Trust source | Examples |
|---|---|---|---|
| Public | Federated, curated | Quality gates, signed manifests | Gmail, Calendar, Slack, Notion |
| Organization | Private to a company | Corporate SSO/SCIM | Internal tools, company wikis |
| Personal | Per-user wallet | User-controlled | Home server, one-off API |

The resolver searches personal → org → public, first match wins, conflicts surfaced with provenance.

### Local discovery broker

The reference implementation ships a **discovery broker** (`ferridis-discovery-broker`, HTTP on `127.0.0.1:7825`) as a process-local service registry that bridges "a service is running" and "the client knows it exists."

**Two registration lifetimes — modelled as an exhaustive enum, not a boolean:**

```rust
enum Lifetime {
    Ephemeral { expires_at: Instant },  // expires after DEFAULT_TTL (300 s) unless refreshed
    Pinned,                              // bypasses TTL; survives process restarts
}
```

Using `Lifetime` rather than `pinned: bool` follows the type-driven discipline (Pattern 5 — exhaustive enums over booleans): an entry is either ephemeral or pinned and neither state can be confused with the other at the type level.

**Wire surface:**

| Endpoint | Purpose |
|---|---|
| `POST /discovery/register` | Register or refresh. Body: `{name, kind, url, persistent?}`. Omitting `persistent` → ephemeral. Backwards-compatible. |
| `GET /discovery/services` | Live snapshot. Each entry includes `"persistent": bool`. |
| `GET /discovery/events` | SSE push stream — fires on every new `POST /register`. |
| `DELETE /discovery/services/{name}` | Remove and atomically persist. Returns 204 (removed) or 404 (not found). |

**Persistence contract:** only pinned entries are written to the state file (`~/.local/share/ferridis/discovery.json`). Ephemeral entries expire naturally and are never revived on restart. Loaded entries are always treated as `Lifetime::Pinned` — being in the state file implies the operator intended the service to survive restarts.

**Broker ↔ client integration:** `Client::discover_broker(url)` fetches the initial snapshot synchronously before returning, so callers can build their tool catalogue immediately. It then subscribes to `GET /discovery/events` in the background and auto-registers new MCP arrivals. `ferridis-mcp-server` activates this subscription at startup when given `--discovery-broker http://127.0.0.1:7825`. The SSE deduplication is intentionally silent — re-registering an already-known service is a no-op.

**Resolved gaps (as of v0.6):**

- **Adapter self-registration** — `ferridis-adapter-sdk::broker` provides a `BrokerRegistration` RAII handle (heartbeat 240 s, abort-on-drop). Adapters call `BrokerRegistration::register(broker_url, ...)` at startup with no manual POST required.
- **stdio MCP bridging** — `ferridis-stdio-bridge` wraps any stdio process and exposes it as an SSE endpoint the broker can register. See [open questions](#open-questions) for remaining governance and vocabulary items.
- mDNS (`_mcp._tcp.local`, `_ferridis._tcp.local`) is scanned but nothing on the network currently advertises on those service types — low priority until the public mesh has real participants.

### Layer 3 — Web-native specs

No new spec language. Capabilities describe themselves in **OpenAPI 3** (request/response shape) or **AsyncAPI** (event-driven). Schemas are validated with **JSON Schema**.

**Why this matters.** Every modern service already publishes one of these, or can with minimal effort. Ferridis becomes a thin layer over existing tooling instead of yet another spec to learn.

### Layer 4 — Bidirectional channels

Request/response gets you call-and-wait. Reactive agents need the world to push back — *the build broke, the email arrived, the calendar event is ten minutes out.* Ferridis treats **push** as first-class, with a consent model that lives at the same level as the connection itself rather than being bolted on per adapter.

**Mechanisms:**

- **Server-Sent Events** for one-way `server → client` push from a connection — the lower-overhead default for pure push channels.
- **WebSockets** for bidirectional channels — subscription management, acknowledgments, and capability-specific control messages travelling upstream alongside the event stream coming down. Both transports share the same `ServerEvent` shape downstream, so consumer code is transport-agnostic.
- **Webhooks** for high-volume, asynchronous events where the runtime cannot keep a long-lived connection open.
- **Manifest-declared event channels** name the channels a capability emits on (one entry per channel, optional `chunk_schema_url`), so the runtime can subscribe only to declared channels and consent maps cleanly onto the names.

**Use cases.** *"Tell me when a high-priority email arrives." "Notify me if the build breaks." "Wake up when my calendar event is in 10 minutes."*

**Consent boundary.** Event subscriptions require the same explicit user consent as the initial connection — and a separate, easily-revocable scope.

### Layer 5 — Intent layer

The intent layer turns goals into capability calls.

**Input:** a structured intent — a verb plus arguments. Example: `send-message(recipient: "Alice", body: "I'll be late")`.

**Process:**

1. Normalize verb against the controlled intent vocabulary.
2. Match against manifests in the mesh (personal → org → public).
3. Surface candidates to the model with confidence scores.
4. Model picks one (or asks the user if ambiguous).
5. Schema for the chosen capability loads on demand.
6. Call executes through the Layer 1 connection.

**Why this matters.** The model doesn't have to memorize 500 tool descriptions. It works at the level of intents; the mesh handles the lookup.

### Layer 6 — Browser / computer-use fallback

Some capabilities will never be wired up natively — legacy enterprise apps, niche internal tools, services without public APIs. For these, the **browser session** and **computer-use vision** are universal escape hatches.

A connection's manifest declares supported tiers (`["native"]`, `["browser"]`, `["browser", "vision"]`, etc.). The runtime picks the highest-precedence supported tier.

**Why this matters.** Ferridis ships useful from day one — even for services that haven't published a manifest yet, the AI can drive them visually if a logged-in browser exists.

### Sub-layer: agent-driving adapters

A specialised case of tier-1 (Native) capabilities: adapters that wrap **other AI agents** as Ferridis capabilities. The reference implementation ships `ferridis-adapter-claude-cli`, which exposes the Claude Code CLI as a stream-kind capability so any Ferridis client (or any MCP-aware host via `ferridis-mcp-server`) can drive a non-interactive Claude Code session — `submit-prompt` to start, `resume-session` to continue by id.

This category exists because editor agent panels (Zed's, Cursor's, others) don't yet expose a public extension API for the active assistant session. Wrapping the CLI is the realistic path to "remote-control Claude Code today"; when editor panels grow a control surface, a thinner adapter can replace this one with the rest of the Ferridis stack unchanged.

## End-to-end walkthrough

User says: *"Tell Alice I'll be 10 minutes late to our 3pm meeting."*

1. **Intent extraction.** The model emits `send-message(recipient: "Alice", body: "Running 10 minutes late, see you soon", relates_to: "today's 3pm meeting")`.
2. **Mesh resolution.** Resolver queries personal → org → public for capabilities supporting `send-message`. Finds three candidates: Slack, Gmail, iMessage. Surfaces all three with confidence scores.
3. **Disambiguation.** Model checks recent context: the most recent communications with Alice are on Slack. Picks Slack.
4. **Schema load.** Slack's full OpenAPI schema for `chat.postMessage` loads on demand.
5. **Connection use.** Runtime fetches the user's Slack connection from the wallet, calls `chat.postMessage` with the bound credential.
6. **Confirmation.** Slack returns a message ID. Runtime emits a structured confirmation to the model and a human-readable status to the user.

Total context cost to the model: the manifests of the three candidate capabilities (~600 tokens) plus the chosen schema (~1200 tokens). The other ~498 capabilities the user has connected are not in context.

![End-to-end walkthrough: "Tell Alice I'll be 10 min late"](./diagrams/walkthrough.svg)


## What Ferridis adds on top of the wire standard

MCP defines the wire-level call. Ferridis adds an opinionated layer above it. The table below shows what those opinions look like — and where each opinion lives in this document — so contributors can see the boundary clearly.

| Dimension | Wire standard (MCP) | Ferridis layer above |
|---|---|---|
| Integration unit | Server process | Connection — an OAuth grant in the user's wallet (Layer 1) |
| User-facing model | Install / configure / run | Authorize once (Layer 1) |
| Discovery | Out of scope at the wire level | Federated three-tier mesh, personal → org → public (Layer 2) |
| Context cost | Full schema in context per tool | Two-stage advertisement: manifest always, schema on demand (Layer 2) |
| Auth | Per-server | OAuth 2.1 / 2.0+PKCE everywhere (Layer 1) |
| Event push | Implementation-defined | First-class bidirectional channels with consent scope (Layer 4) |
| Fallback path | Not defined | Browser + computer-use as tier 3 of the same protocol (Layer 6) |
| Trust model | Per-server | Three-tier registry with signed manifests and provenance surfacing (Layer 2) |
| Spec language | JSON-RPC over MCP transports | OpenAPI 3 / AsyncAPI 2.6+ at the manifest's `schema.url` (Layer 3) |
| Composition | Caller composes | Intent layer routes goals across capabilities (Layer 5) |
| Reference implementation | TypeScript, Python | Rust, with typestate + illegal-states-unrepresentable |

None of these displace MCP. They are the opinions Ferridis takes on top of it.

## Resolved frictions

Three design tensions were worked through before this document was written. Brief summary:

1. **Intent vs. lazy loading** → two-stage manifest/schema advertisement.
2. **Browser vs. native precedence** → declared tier preference, user-overridable.
3. **Public vs. private registry** → three-tier mesh (personal/org/public).

Full record: [architecture-frictions.md](./architecture-frictions.md).

## Open questions

Design tensions that remain unresolved:

- **Intent vocabulary governance.** Drafted in [`vocabulary.md`](./vocabulary.md) (35 verbs across 8 categories) and proposed for AAIF. Until AAIF accepts (or refers elsewhere), George maintains. The most important governance question in the project remains *which body owns the namespace long-term*.
- **Cross-capability transactions.** What happens when an intent needs two capabilities atomically (e.g., "move this email to a folder *and* archive the calendar event it created")? Out of scope for v0.x, important for v1.
- **Local capability execution.** Connections that run locally (a script on the user's machine) need a different trust model than remote ones. Sketched but not specified.
- **CT log validation.** CRL-based revocation ships in v0.6; Certificate Transparency log verification (checking that the signing cert appears in a CT log) is not yet implemented. Lower urgency since Rekor already provides an append-only inclusion proof.

## Resolved since v0.1

Answers to questions that were open in earlier revisions:

- **Manifest signing.** Sigstore (cosign-bundle JSON). Full trust chain — sig-vs-cert + cert window + Rekor inclusion proof + Fulcio chain via `rustls-webpki` + CRL revocation check. Ships in `ferridis-protocol::signing`. `TrustRoot::from_sigstore_tuf` auto-fetches trust material from Sigstore's Public Good TUF repo.
- **Streaming responses.** Manifest declares per-intent `kind: "request" | "stream"` plus `chunk_schema_url`; wire transport reuses SSE; consumer API is `Client::dispatch_streaming -> Pin<Box<dyn Stream<Item = …>>>`.
- **Bidirectional event channels.** WebSocket primitive in `ferridis-protocol::ws` with typed `ClientMessage` (`Subscribe` / `Unsubscribe` / `Ack` / `Control`) upstream and reused `ServerEvent` downstream.
- **Adapter self-registration.** `ferridis-adapter-sdk::broker` ships a `BrokerRegistration` RAII handle with 240 s heartbeat — adapters call it at startup, no manual POST required. (v0.6)
- **stdio MCP bridging.** `ferridis-stdio-bridge` wraps any stdio MCP server as an HTTP SSE endpoint the broker can register. Deployed to handle tools like `linux-health-mcp` that have no network URL. (v0.6)
- **Cert revocation.** CRL-based revocation check against the CDP extension in the signing cert. `RevocationMode::BestEffort | Required | Skip` is caller-configurable. CT log verification deferred (see open questions). (v0.6)

---

## Workspace code architecture

Everything above describes the *protocol*. This section describes the *Rust workspace* that implements it — crate topology, parsing boundaries, and the decisions log. (It lives in this file rather than a separate `ARCHITECTURE.md` because the repository must build on case-insensitive filesystems, where the two names collide.)

### Crate topology

Fifteen crates in `rust/crates/`, layered strictly — a crate depends only on crates in the rows above it:

| Layer | Crates | Role |
|---|---|---|
| Types | `ferridis-core` | Pure types, no I/O. Typestate `Connection<S>`, parse-don't-validate `Manifest`, validated `IntentVerb` / `Tier` / `Tiers` / `AccessToken`, `IntentKind`, `EventChannel`, `ChannelTransport`. |
| Wire | `ferridis-protocol` | HTTP transport, OAuth 2.0+PKCE, manifest/schema fetch, mesh + federated mesh + signed mesh-index, Sigstore trust chain (TUF auto-fetch, CRL revocation), SSE events + streaming, WebSocket bidi, mDNS scanning. |
| Frameworks | `ferridis-adapter-sdk` (publisher), `ferridis-client` (consumer) | SDK: `Capability` trait, axum `AdapterServer`, kind-aware routing, request validation, `WsHandler`, event publishing, broker self-registration. Client: keychain wallet, three-tier registry, intent matching, auth-aware + streaming dispatch, schema validation, MCP-consumer module, mesh/broker/mDNS registration. |
| Adapters | `ferridis-adapter-fs`, `-claude-cli`, `-google-calendar`, `-slack`, `-github`, `-notion` | Reference publishers. Each embeds its manifest and implements `Capability` (and `dispatch_stream` where stream-kind). |
| Model hosts | `ferridis-ptc` | Programmatic tool calling over the Messages API: projects the client catalogue into custom tools with per-tool `allowed_callers`, drives the pending-calls typestate, and gates every call on its caller policy before `Client::dispatch_from`. Library plus the `ptc_run` example. |
| Binaries | `ferridis-cli`, `ferridis-mcp-server`, `ferridis-discovery-broker`, `ferridis-stdio-bridge` | Sidecar (JSON-RPC over stdio), MCP-publisher shim, local service registry (port 7825), stdio-MCP-to-SSE bridge (port 7826). |

Default adapter ports: fs 7821, claude-cli 7823, github 7827, google-calendar 7828, slack 7829, notion 7830.

### Parsing boundaries

Every external input is parsed into a witness type at exactly one perimeter; interior code takes witnesses and never re-checks:

- **JSON off the wire** → `serde(try_from = "String")` on every validated newtype (`IntentVerb`, `Category`, `Summary`, `CapabilityVersion`, `CapabilityRef`). Deserialization *is* the validating constructor.
- **Manifests** → parsed on receipt in `ferridis-protocol::manifest`; everything downstream assumes validity.
- **Connection lifecycle** → typestate `Connection<Pending | Authorized | Expired | Revoked>`; transitions consume `self`. Reconstruction from storage goes through `pub(crate)` ctors inside `ferridis-core` only.
- **Filesystem paths** (fs adapter) → `Root` / `RelPath` typestate; a path that escapes the root is unrepresentable.
- **Claude CLI inputs** → `Prompt`, `AllowedCwd`, allow-list-gated `Model`, UUID-validated `SessionId` at the dispatch boundary.
- **Secrets** → `AccessToken` with redacted `Debug`; wallet entries live in the OS keychain, never plaintext.
- **Request bodies** → validated twice by design: client-side against `input_schemas[intent]`, adapter-side via `Capability::body_schema` (the adapter is the final validator).

Workspace-wide discipline: no `unsafe`, `#![deny(missing_docs)]` on every public crate, clippy clean with `-D warnings`, toolchain pinned 1.95.0 / Edition 2024. [FEATURES.md](./FEATURES.md) records the enforcing artifact (type or test) for every shipped feature.

### Decisions log (append-only)

New entries go at the bottom with date, decision, why, and what was rejected. Never rewrite old entries.

| Date | Decision | Why | Rejected alternatives |
|---|---|---|---|
| v0.1 | Rust, Edition 2024, MSRV 1.95, toolchain pinned | Protocol-grade compile-time guarantees; reproducible builds | TypeScript/Python-style references (weaker guarantees) |
| v0.1 | `rustls` everywhere, no OpenSSL | Portable builds, no system-dependency footgun | OpenSSL linkage |
| v0.1 | HTTP pool hardening (`pool_idle_timeout` 15 s, `pool_max_idle_per_host` 2, `connect_timeout` 5 s) | Stale localhost keepalives survived adapter restarts and wedged calls (hit in the Task 11 demo) | Default reqwest pool settings |
| 2026-05-11 | OS keychain wallet; **refuse to run** without a backend; refuse legacy plaintext wallets | A silent plaintext fallback is a downgrade vector; explicit `FERRIDIS_WALLET_MEMORY=1` is the only escape hatch | Plaintext fallback with a warning |
| 2026-05-11 | `serde(try_from = "String")` on validated newtypes | `serde(transparent)` silently bypassed the validating constructors | Keeping transparent + re-validating later (scattered checks) |
| 2026-05-11 | Publisher registers adapters partial-failure-tolerant, 10 s per-adapter timeout; `tools/list` advertises the working subset only | All-or-nothing stalled startup on one dead host; a tool guaranteed to fail is worse UX than one that isn't there | Error-placeholder tools; unbounded TCP connect window |
| 2026-05-11 | MCP `inputSchema` preserved verbatim through the projection | Typed args (integer `limit`) must round-trip; regenerated schemas are lossy | Hardcoded per-tool schema tables as the primary source |
| 2026-05-12 | **Sigstore** (cosign + Fulcio + Rekor) for manifest signing | Web-native, keyless, existing transparency log — reuse, don't invent key distribution | Bare Ed25519 publisher keys; self-managed X.509 |
| 2026-05-12 | Streamed intents must declare `chunk_schema_url` (validation error otherwise) | A silent default is a footgun for chunk-validating clients | Optional field with an implicit any-schema |
| 2026-05-12 | MCP SSE session-loss auto-recovery: typed `McpSessionExpired`, `reconnect()` + replayed `initialize`, retry exactly once | Server restarts left long-running consumers POSTing to dead sessions with no recovery path | Failing the call and requiring a host restart |
| 2026-05-13 | Publisher branches on `intent_kind()`: stream-kind collects chunks via `dispatch_streaming`, returns final result + chunk array | MCP 2024-11-05 hosts expect a single response; progressive forwarding waits for notifications-as-chunks | Rejecting stream-kind tools from the publisher |
| 2026-05-23 | `Lifetime` enum (`Ephemeral { expires_at }` \| `Pinned`) in the discovery broker, replacing a `persistent: bool` | The bool + TTL pair could disagree — illegal states unrepresentable instead; state file stores only pinned entries | Boolean flag beside an expiry field |
| 2026-05-24 | `RevocationMode { BestEffort \| Required \| Skip }` for Sigstore verification, default `BestEffort` | CRL endpoints are flaky; callers choose strictness explicitly | Hard-required CRL (breaks offline use); silent skip |
| 2026-07-20 | This section + decisions log added; FEATURES.md converted to an enforced-by ledger | Context files fail two ways — staleness and unenforced guarantees; the ledger and log exist to kill both | Feature matrix without enforcing artifacts (status quo) |
| 2026-07-20 | Backpressure wire shape: separate `event: backpressure {"signal": …}` emitted on state *change*; `Halt` terminates the stream with no trailing `end`; SDK entry point is `Capability::dispatch_stream_flow` with a default that adapts `dispatch_stream` | Additive on the wire (pre-v0.7 clients ignore unknown SSE events) and additive in the API (existing adapters compile and behave identically); halt-as-terminator keeps "deliberate stop" distinguishable from both clean `end` and dropped connection | Embedding the signal inside each `chunk` payload (breaks the chunk shape for old clients); changing `IntentStream`'s item type (breaks every existing adapter) |
| 2026-07-20 | Stream cancellation stays connection-scoped: consumer drop closes the HTTP connection, the SDK drops the adapter's intent stream, and resource cleanup rides `Drop` (`kill_on_drop` children etc.) — no explicit cancel message on the SSE path | SSE is one-way; the connection close *is* the cancel signal, and Rust's `Drop` makes the cleanup guarantee compile-time-structural rather than protocol-dependent. Enforced by `consumer_drop_reaches_the_adapter_stream_drop`. | An out-of-band `DELETE /intents/...` cancel endpoint (second code path to keep consistent; the WS transport's typed `Unsubscribe` already covers the bidi case) |
| 2026-10-25 | **Approved; shipped in v0.7.0. (PTC-1) Caller policy for programmatic tool calling.** Type: `CallerPolicy { Direct \| Code }` in `ferridis-core`. Never both, following Anthropic's guidance to pick one per tool. Three sources resolve to one `EffectiveCallerPolicy`, highest first: **(1) constraints Ferridis computes** (a recursive `$ref` in the input schema forces `Direct` and logs a WARN naming the tool; nothing overrides this); **(2) operator override** in adapters.json (a per-tool map, keyed by tool name, so MCP-backed entries can use it too); **(3) publisher declaration**, an optional `"callers": "direct" \| "code"` on a structured manifest intent entry (protocol v0.7, additive); **(4) default `Direct`**. Enforcement happens in `ferridis-ptc` at dispatch and goes both ways: a programmatic call to a `Direct` tool, or a direct call to a `Code` tool, gets an `is_error` tool_result and never reaches `Client::dispatch`. | `allowed_callers` is not a security boundary (Anthropic docs, re-verified 2026-10-25), so the gateway enforces the policy. Both sources are needed. Only the publisher knows which intents are safe to fan out from a loop (`list-*` yes, `delete-*` no). Only the operator can speak for the MCP-federated tools, whose manifests are synthetic projections: MCP 2024-11-05 carries no such field, so a manifest-only design leaves every federated tool at the default. This is the shape of resolved friction #2: the publisher declares and the user overrides. The new field is additive on the wire: `RawIntentEntry` is `untagged` without `deny_unknown_fields`, so pre-v0.7 parsers ignore it and fall back to the API's own default, `Direct`, which is the safe direction. | Manifest-only (federated MCP tools could never be marked `Code`). adapters.json-only (the publisher's safety opinion is lost, and every operator has to re-derive it per tool). `Direct + Code` as a third variant (the docs advise against it, and it makes "which surface is this tool for" ambiguous). A `bool programmatic` (Pattern 5). A sentinel "undeclared" value: undeclared is `Option::None` at the manifest layer and is resolved to a variant before dispatch. Leaving enforcement to `allowed_callers` alone. |
| 2026-10-25 | **Approved; shipped in v0.7.0. (PTC-2) Where response schemas for `describe()` come from.** Recommended: (B) convert the six reference adapters' embedded OpenAPI documents from YAML to JSON (OpenAPI 3 allows either), author a `responses.200.content.application/json.schema` for each intent, and fetch and parse `schema_url` once at registration in `ferridis-client`, using the same 10 s bound as registration. A schema that is not JSON, missing, or fails to parse logs a WARN, and the description then carries "Returns JSON" without a schema. | The brief assumed Ferridis already holds each manifest's OpenAPI document. It does not. The manifest stores only `schema_url`. `fetch_schema` returns opaque bytes and nothing calls it at registration. All six embedded documents are YAML, and the workspace has no YAML parser. Every intent's response is `'200': { description: OK }`, with no response schema anywhere. MCP-backed tools cannot supply one either, because 2024-11-05 has no `outputSchema`. JSON keeps this at zero new dependencies. | (A) Keep YAML and add a YAML crate (`serde_yaml` is archived, and the forks have to be priced per `[R:price-every-dependency]`). (C) A per-intent `response_schema_url` in the manifest (duplicates OpenAPI, and the project rule is "web-native, do not invent"). Appending a schema that nothing produces (inert, `[R:wired-artifact]`). |
| 2026-10-25 | **Approved; shipped in v0.7.0. (PTC-3) One tool projection shared by both surfaces.** Move the catalogue logic out of the `ferridis-mcp-server` binary into a new `ferridis-client::catalogue` module: tool naming, `describe()`, and input-schema precedence, plus `AdapterConfig` and `register_adapters` so the `ferridis-ptc` example can read the same adapters.json. `ferridis-mcp-server/src/tools.rs` becomes a thin MCP-shape projection over it, and `ferridis-ptc` adds a Messages-API projection. | `ToolCatalogue` and `describe()` live in a binary crate, so `ferridis-ptc` cannot import them. The tool name for an intent has to be the same on the MCP surface and the PTC surface, otherwise one intent gets two identities and the operator's per-tool overrides drift between them. The existing mcp-server e2e suite drives the real binary, so it guards the move without changes. **This exceeds the brief's "no change to ferridis-mcp-server beyond describe()", so it needs explicit approval.** | Re-implementing the projection inside `ferridis-ptc` (two naming functions that will drift). Making `ferridis-mcp-server` lib+bin and depending on it (pulls axum and the OAuth server into the PTC crate). |
| 2026-10-25 | **APPROVED by the maintainer as recommended: PTC-1, PTC-2, PTC-3** (the three rows above), including the two-way enforcement in PTC-1. | Maintainer decision. | See the three rows above. |
| 2026-10-25 | **(PTC-2 refinement) Response schemas are fetched from the adapter first.** For a Native backend, fetch `{base_url}/schema`, the route `ferridis-adapter-sdk` serves for every adapter. Fall back to the manifest's `schema_url` only if that fails. | Found during implementation: every reference manifest's `schema.url` points at `ferridis.io` or `ferridis.dev`, and neither resolves (checked 2026-10-25). Fetching `schema_url` alone would have produced no schema for any adapter. The adapter-served copy is also the one that matches the running adapter's version. The SDK's `Redirect` source still reaches a canonical URL, because reqwest follows redirects. | Fetching `schema_url` only (fails for all six). Rewriting the manifests to carry the local URL (the base URL differs per deployment, and a manifest is static). |
| 2026-10-25 | **(PTC-2 refinement) Response schemas load on request, not at registration.** `Client::load_response_schemas` is a separate, explicit call; `Client::register` never fetches the OpenAPI document. Callers that must describe every tool up front opt in. Today that is `ferridis-mcp-server` registration, under the same 10 s per-adapter bound, for configured and broker-discovered adapters alike. | Resolved friction #1 makes schemas lazy, so a host that only dispatches should never pay for a document it does not read. A failed load returns a typed `ResponseSchemaLoad::NoneFound` listing every candidate and why it failed. It never fails registration, because a missing response schema only makes a description less specific. | Loading inside `register` (eager for every host, breaks friction #1). Failing registration when no schema loads (one unreachable document would remove a working adapter's tools). |
| 2026-10-25 | **(PTC-1 refinement) The adapters file gains an object form.** `{"adapters": [...], "callers": {"<tool name>": "direct" \| "code"}}`, alongside the original bare array, which still parses unchanged. Unknown top-level keys are rejected; `callers` keys are parsed as `ToolName`; a key that matches no registered tool is WARNed by `ToolPlan::build`, not rejected, because the set of tools is only known after registration and a federated server may be down. | The override has to live beside the adapters it overrides, and every existing file is a bare array (the VS Code extension shares the shape). `deny_unknown_fields` on the object form turns a misspelt `caller` key into a load error instead of a silently ignored override. | A separate overrides file (two files to keep in step). Rejecting unknown tool names at load (one offline MCP server would stop the host starting). |
| 2026-10-25 | **(PTC-1 refinement) An unresolvable `$ref` downgrades like a recursive one.** External references, dangling JSON pointers and anchor-only fragments make a code tool direct-only, with the same WARN. The code-execution tool is offered only when at least one tool resolves to `Code`. | Ferridis cannot prove an external or dangling reference acyclic, and the API rejects circular references for programmatic tools with a 400 that fails the whole request, not one tool. Offering code execution with no code-callable tool would start a container for nothing. | Fetching external schemas to check them (network at plan time, and a new trust question). Always offering code execution. |
| 2026-10-25 | **The caller reaches tier-used telemetry through a span, not a new log field.** `Client::dispatch_from(origin, …)` and `dispatch_streaming_from` wrap the existing dispatch in `info_span!("ferridis_dispatch", caller = %origin)`; `CallOrigin { Host, ModelDirect, ModelCode }`. `dispatch` is unchanged (and unspanned). | The tier-used `info!` is emitted from six places across the native, MCP and streaming paths. A span adds the field to all of them from one site, and to any future emit site, with no signature change on the existing API. | Threading `origin` through every internal dispatch function into each `info!` (six edits that drift). A separate "call made" event (two records to join). |
| 2026-10-25 | **`ferridis-ptc` failure policy.** A call with an unrecognised `caller.type` is refused, never dispatched. A run is bounded by `MaxTurns` (default 16). A `refusal` stop ends the run as a final turn: server-side refusal fallback is **not enabled**. Requests are non-streaming with `max_tokens` 16 000 and no `thinking` field (the default model's thinking cannot be disabled). | Fail closed on what the gate cannot classify. A model that keeps calling tools spends without bound unless something counts turns. Fallback changes which model answers, which is the operator's decision, not a library default. | Treating unknown callers as direct. Unbounded runs. Enabling fallback by default. |
| 2026-10-25 | **Replay fixtures are transcribed, not recorded.** The wiremock responses in `ferridis-ptc/tests/replay.rs` are written from the documented programmatic-tool-calling shapes; no live response has been captured yet. The `#[ignore]`'d live smoke test is the check that the documentation matches the wire. | Recording needs a paid live run, which is the maintainer's call (`[R:no-silent-spend]`). Saying so keeps the replay suite from reading as evidence about the live API. | Labelling the fixtures "recorded". Running the live test unasked. |
| 2026-10-26 | **`ferridis-ptc` names a workspace when one is configured.** `PtcClient::with_workspace(WorkspaceId)` sends `anthropic-workspace-id` on every request; with none configured the header is absent. The live test and `ptc_run` read `ANTHROPIC_WORKSPACE_ID`, the variable the Anthropic CLI uses. The live smoke test then passed (5 turns, 4 programmatic calls, about $0.05), so the transcribed replay shapes match the wire for the paths it exercised. | The first live attempt was refused with a 400: a personal or service-account key not scoped to a workspace must name one on every request. `Option<WorkspaceId>` because "no workspace" is a real state (a workspace-scoped key needs none). | Requiring a workspace-scoped key (an unscoped key is a documented key type, and the failure only shows on the live path). `PtcClient::new` reading the variable implicitly (`WorkspaceId::from_env` exists, but the host decides whether to call it, as with `ApiKey::from_env`). |
| 2026-10-26 | **`ferridis-ptc` retries transient failures.** `RetryPolicy` (default 2 retries, 0.5 s doubling to 8 s, the official SDKs' defaults) retries `429`, any `5xx` and connection failures; `retry-after` is honoured up to 60 s and a longer one returns the error. Container expiry is re-checked before each retry. No jitter. | The documented retryable set. A connection failure cannot have reached the API, so resending is safe; a timeout or a dropped response may have been processed, so it is returned. A `retry-after` beyond a minute means the limit will not lift within the run. One client retrying twice synchronises with nothing, so jitter would cost a dependency for no benefit. | Retrying timeouts (a 600 s request, possibly processed, sent again). Capping `retry-after` at the backoff cap (retrying before the server allows fails again). Adding `rand` for jitter. |
| 2026-10-26 | **Tool results are capped per caller, and an oversized result is an error, not a truncation.** `MaxResultBytes`: 128 KiB for direct results, 1 MiB for results going to code; `Executor::with_max_result(CallerPolicy, …)`. | Truncated JSON does not parse in the code that asked for it; an error naming the size lets the model or its code ask for less. A single limit would either let direct results run up token cost or refuse the large intermediate results that programmatic calling exists to keep out of the model's context. Results to code still travel in every later request, so they need a bound too. | Truncating with a marker. One limit for both callers. No limit on results to code. |
| 2026-10-26 | **Each dispatch has a deadline.** `CallDeadline` (default 180 s) wraps the dispatch in `tokio::time::timeout`; on expiry the call is answered with an error result saying it may still have taken effect. Answering calls and retrying now need a tokio runtime with time enabled. | The API gives up on a paused programmatic call after about four minutes; answering first lets the code handle the failure instead of the container timing out. Dropping the dispatch cancels the HTTP request, but the adapter may already have acted, so the message must not claim it did not. | Waiting for the 600 s request timeout. A deadline computed from the container's `expires_at` (that bounds the container, not the pending call). |
| 2026-10-26 | **The VS Code extension gains a service mode (v0.3).** `ferridis.serverUrl` makes VS Code connect to an already-running `ferridis-mcp-server --http-bind` through `vscode.lm.registerMcpServerDefinitionProvider`. The bearer is kept in VS Code `SecretStorage` and added as a header in `resolveMcpServerDefinition`, so it exists only in memory when a server starts. `serverUrl` takes precedence over `serverPath`, and the v0.2 stdio entry is removed in service mode. Settings are parsed once into a `Connection` union in a module with no `vscode` import (`src/connection.ts`), so the rules run under `node --test`. Plain `http` is accepted only for a loopback host. | Where the server already runs as a service, a second stdio copy per VS Code window duplicates every child adapter. It also runs as the user, so any adapter needing the service's privilege is skipped: on the maintainer's machine that is 6 of 112 tools. The bearer belongs in secret storage because every settings-based route puts it in a plain-text file that roams with settings sync. Cleartext http to a remote host would send that bearer unencrypted. | *Writing an `http` entry with an `Authorization` header into `mcp.servers`*: the token in plain-text `settings.json`. *An `mcp.json` `inputs` password prompt*: secret-backed, but configuration the extension would have to author in a second file, and it prompts per start rather than once. *Stdio only, pointed at the service binary*: the duplicate federation above. *Raising the minimum VS Code version silently*: recorded instead. The provider API arrived in 1.101, so `engines.vscode` moves from `^1.99.0` to `^1.101.0`. |
| 2026-10-26 | **Hosted CI switched off; the gate runs locally.** GitHub Actions is disabled in the repository settings, the CI workflow is disabled, and `ci.yml` keeps its job but triggers only on `workflow_dispatch`. `scripts/check.sh` runs the same three stages with the same flags, runs every stage even after a failure, and prints each stage's exit status. | Maintainer decision: no exposure to Actions billing. Keeping the workflow file means bringing CI back is a settings switch plus two trigger lines, not a rewrite. The two-OS coverage CI gave is now the maintainer's job: the script says to run it on Linux and on Windows. | Deleting `ci.yml` (loses the tested matrix and the reasons in its comments). Disabling only the workflow (any other workflow added later would run). A self-hosted runner (still Actions, and a long-lived process on the maintainer's machine). |
| 2026-10-26 | **`ferridis-mcp-server --bearer-token-file <path>`, refused beside any other bearer source.** The file holds one token; surrounding whitespace is trimmed and inner whitespace refused. Errors name the path and a length, never the token. | A service manager launching the server had to hold the token in its own config, which for WinSW is a plain-text XML. On the maintainer's machine that file was printed whole during a survey and the token reached a transcript, so it had to be rotated. A path in the config and the token in one file with tight permissions moves the secret out of the file that gets read for other reasons. Refusing a file beside `--bearer-token` or `FERRIDIS_MCP_BEARER` rather than ranking them: whichever source lost would be a credential the operator believes is in force and is not, and a stale `<env>` entry left behind after a switch is exactly that case. | *Ranking the sources* (file over env, or env over file): the silent-loser above. *Reading the token from the OS credential store*: under LocalSystem that is SYSTEM's store, which the operator cannot easily populate or inspect, and it would add a second keychain path beside the wallet's. *Checking the file's ACL at startup*: Windows ACLs and Unix modes need separate code, and a warning nobody reads protects nothing; INSTALL.md gives the `icacls` line instead. |
| 2026-10-26 | **The discovery broker's default state path: `$HOME/...`, else `%LOCALAPPDATA%\Ferridis\discovery.json`, else refuse to start.** `state_path::default_state_path` takes the environment as a function, so the choice is tested without mutating the process environment. | With `HOME` unset the old code used `"."`, a sentinel that silently meant "the working directory": `C:\Windows\System32` for a Windows service, which is where registrations would have been persisted, or failed to persist. `HOME` stays first so existing installs keep their file. | *A `dirs`-style crate*: a dependency for two lookups, and it would move existing Linux installs off `~/.local/share` if its answer differed. *Falling back to the executable's folder*: works for a portable install, but silently differs between a service and an interactive run of the same binary. |
| 2026-10-26 | **`ferridis-ptc` caches every request.** A top-level automatic `cache_control` (its breakpoint moves with the conversation) plus one explicit breakpoint on the last tool definition. `Usage` keeps uncached input, cache write and cache read apart. | Measured on the maintainer's 112-tool catalogue: the tool definitions are about 25,000 input tokens, re-sent on every turn. Uncached, a 5-turn run spends about $0.60 on definitions alone. With caching the definitions are paid once at 1.25× and read at a fraction on later turns, and on later runs within five minutes. The tools breakpoint is separate so a run that diverges early still reads the tools prefix. | *Caching only the tools*: later turns would re-pay the growing conversation. *A 1-hour TTL*: costs 2× to write and pays back only on three or more requests an hour apart, which is not this workload. *Trimming the tools sent*: changes what Claude can do; caching changes only the price. |
| 2026-10-26 | **`ferridis-mcp-server` offers programmatic tool calling as one MCP tool, `ferridis_ptc_run`.** The tool exists only with `--ptc-api-key-file`; it takes `{"task"}`, runs one `ferridis-ptc` conversation over the server's own registered tools and the adapters file's `callers` map, and returns the final answer plus a summary line. Runs go one at a time; a catalogue tool of the same name stops startup. This widens PTC-3 (which kept `ferridis-mcp-server` to the catalogue move); the maintainer chose it on 2026-10-26. | MCP clients, the editors included, call tools one at a time and cannot host a Messages API conversation, so without this PTC reached only Rust hosts. Running the conversation inside the publisher reuses the client, wallet and caller policy it already has; a separate binary would duplicate all three. One run at a time bounds what concurrent calls can spend and keeps each run's call counts exact. Its own key file, rather than an environment variable, for the same reason as `--bearer-token-file`. Refusing a name collision rather than letting one shadow the other: either winner would be a tool the operator believes is reachable and is not. | *A separate `ferridis-ptc-server`*: a second process, port and bearer to deploy, and a second registration of every adapter. *Teaching the VS Code extension to run PTC*: TypeScript reimplementation of the gate and policy, and the key would live in the editor. *Concurrent runs*: unbounded spend from a chat client that retries. |
