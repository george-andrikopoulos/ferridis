//! The top-level [`Client`] — embedding hosts' entry point.
//!
//! `Client` composes the three pieces of consumer-side state — the
//! [`Wallet`], the [`Registry`], and a shared HTTP client — and
//! exposes the four host-facing operations:
//!
//! - [`register`](Client::register) — fetch a capability's manifest
//!   and remember where to dispatch its intents.
//! - [`candidates_for_intent`](Client::candidates_for_intent) — find
//!   the registered capabilities that declare a given intent.
//! - [`insert_connection`](Client::insert_connection) — store a
//!   [`StoredConnection`] in the wallet (typically after OAuth has
//!   completed, or in tests).
//! - [`dispatch`](Client::dispatch) — invoke an intent against a
//!   chosen capability, honouring its declared auth method.
//!
//! # Concurrency
//!
//! `Wallet` and `Registry` are guarded by [`tokio::sync::Mutex`]; the
//! `Client` is `Send + Sync + Clone` (it holds `Arc`s internally) so
//! it can be shared across editor threads, async tasks, or sidecar
//! IPC handlers without further wrapping.

use std::sync::Arc;

use ferridis_core::{AuthMethod, CapabilityRef, IntentVerb, Manifest, StoredConnection};
use ferridis_protocol::{
    CallRequest, CallResponse, Client as HttpClient, FederatedMesh, MeshClient, OpenApiError,
    ResponseSchemas, ServerEvent, TrustRoot, WsConnection, call as call_authed, call_anonymous,
    connect_ws, fetch_manifest, subscribe as protocol_subscribe,
};
use futures_util::Stream;
use reqwest::Method;
use tokio::sync::Mutex;
use tracing::Instrument;
use url::Url;

use crate::error::ClientError;
use crate::registry::{CapabilityRecord, Registry};
use crate::wallet::Wallet;

/// Consumer-side Ferridis client.
///
/// Cheap to [`Clone`] — the wallet, registry, and HTTP client are all
/// reference-counted internally.
#[derive(Clone)]
pub struct Client {
    wallet: Arc<Mutex<Wallet>>,
    registry: Arc<Mutex<Registry>>,
    http: HttpClient,
}

impl Client {
    /// Open a keychain-backed wallet under `namespace` and build a
    /// client around it. Returns an error if the OS keychain is not
    /// reachable; **never falls back to plaintext**.
    pub fn open_keychain(namespace: impl Into<String>) -> Result<Self, ClientError> {
        let wallet = Wallet::open_keychain(namespace)?;
        Ok(Self::with_wallet(wallet))
    }

    /// Build a client with an in-memory wallet. **Test / embedded use
    /// only.** Production callers must use
    /// [`Self::open_keychain`].
    pub fn ephemeral() -> Self {
        Self::with_wallet(Wallet::ephemeral())
    }

    /// Build a client around an existing wallet.
    pub fn with_wallet(wallet: Wallet) -> Self {
        Self {
            wallet: Arc::new(Mutex::new(wallet)),
            registry: Arc::new(Mutex::new(Registry::new())),
            http: HttpClient::new(),
        }
    }

    /// Borrow the underlying wallet (for explicit save / inspection).
    pub fn wallet(&self) -> &Arc<Mutex<Wallet>> {
        &self.wallet
    }

    /// Borrow the underlying registry (for inspection / advanced use).
    pub fn registry(&self) -> &Arc<Mutex<Registry>> {
        &self.registry
    }

    /// Register a capability with this client.
    ///
    /// `manifest_url` is the URL the manifest JSON lives at — typically
    /// `<adapter-base>/manifest.json` for a Ferridis adapter, or a
    /// well-known public-mesh URL for a federated capability.
    /// `base_url` is the root the adapter serves intent endpoints from;
    /// dispatch builds call URLs as `{base_url}/intents/{verb}`.
    ///
    /// The manifest is fetched, validated through
    /// [`ferridis_core::Manifest::parse`] (in [`fetch_manifest`]), and
    /// stored in the registry. The capability's intent set becomes
    /// immediately resolvable via [`candidates_for_intent`].
    pub async fn register(
        &self,
        capability: CapabilityRef,
        manifest_url: Url,
        base_url: Url,
    ) -> Result<Manifest, ClientError> {
        let manifest = fetch_manifest(&self.http, &manifest_url).await?;
        let record = CapabilityRecord::new(capability.clone(), manifest.clone(), base_url);
        self.registry.lock().await.insert(record);
        tracing::info!(
            capability = %capability,
            "registered capability with personal-tier registry"
        );
        Ok(manifest)
    }

    /// Read per-intent response schemas from `capability`'s OpenAPI
    /// document and attach them to its registry record.
    ///
    /// Registration never calls this. Schemas stay lazy (resolved
    /// friction #1), and only callers that must describe every tool up
    /// front, such as a tool catalogue, opt in.
    ///
    /// Candidates, in order (decision PTC-2 and its refinement in
    /// `architecture.md`):
    ///
    /// 1. `{base_url}/schema`, the route `ferridis-adapter-sdk` serves for
    ///    every adapter. It matches the running adapter's version.
    /// 2. The manifest's `schema_url`.
    ///
    /// A failed candidate is not an error. The outcome lists each attempt
    /// and its failure, and each one is logged at WARN, because a missing
    /// response schema only makes a tool description less specific.
    ///
    /// # Errors
    ///
    /// [`ClientError::CapabilityNotRegistered`] if `capability` is unknown.
    pub async fn load_response_schemas(
        &self,
        capability: &CapabilityRef,
    ) -> Result<ResponseSchemaLoad, ClientError> {
        let candidates: Vec<Url> = {
            let registry = self.registry.lock().await;
            let record = registry
                .get(capability)
                .ok_or_else(|| ClientError::CapabilityNotRegistered(capability.clone()))?; // allow:clone — the error variant owns the ref it names; only on the failure path
            let base_url = match record.backend() {
                crate::registry::CapabilityBackend::Native { base_url } => base_url,
                crate::registry::CapabilityBackend::Mcp { .. } => {
                    return Ok(ResponseSchemaLoad::NotApplicable);
                }
            };
            let mut candidates = Vec::with_capacity(2);
            match base_relative_url(base_url, "schema") {
                Ok(url) => candidates.push(url),
                Err(e) => {
                    tracing::warn!(capability = %capability, error = %e, "cannot build adapter /schema URL")
                }
            }
            let declared = record.manifest().schema_url();
            if !candidates.contains(declared) {
                candidates.push(declared.clone()); // allow:clone — the URL must outlive the registry lock released at the end of this block
            }
            candidates
        };

        let mut attempts = Vec::with_capacity(candidates.len());
        for url in candidates {
            let parsed = match ferridis_protocol::fetch_schema(&self.http, &url).await {
                Ok(bytes) => ResponseSchemas::from_openapi_json(bytes.body())
                    .map_err(SchemaFetchFailure::Parse),
                Err(e) => Err(SchemaFetchFailure::Fetch(Box::new(e))),
            };
            match parsed {
                Ok(schemas) => {
                    let intents = schemas.len();
                    self.registry
                        .lock()
                        .await
                        .set_response_schemas(capability, schemas)?;
                    tracing::info!(capability = %capability, from = %url, intents, "loaded response schemas");
                    return Ok(ResponseSchemaLoad::Loaded { from: url, intents });
                }
                Err(failure) => {
                    tracing::warn!(
                        capability = %capability,
                        url = %url,
                        error = %failure,
                        "response-schema candidate failed; tool descriptions will omit the schema unless a later candidate succeeds"
                    );
                    attempts.push(SchemaAttempt { url, failure });
                }
            }
        }
        Ok(ResponseSchemaLoad::NoneFound { attempts })
    }

    /// Capabilities whose cached manifest declares `intent`.
    pub async fn candidates_for_intent(&self, intent: &IntentVerb) -> Vec<CapabilityRef> {
        self.registry.lock().await.candidates_for_intent(intent)
    }

    /// Insert (or replace) a connection in the wallet.
    /// With the v0.2 keychain-backed wallet, each insert is atomic at
    /// the store level and persisted immediately — no explicit `save`
    /// is required.
    pub async fn insert_connection(&self, stored: StoredConnection) -> Result<(), ClientError> {
        let mut w = self.wallet.lock().await;
        w.insert(stored)?;
        Ok(())
    }

    /// Invoke `intent` against `capability` with the given JSON body.
    ///
    /// The dispatch flow:
    ///
    /// 1. Look the capability up in the registry. Error if not registered.
    /// 2. Check that the manifest declares this intent. Error if it does not.
    /// 3. If the capability advertises a typed `inputSchema` for this
    ///    intent (carried on the registry record), validate the body
    ///    against it. Reject with [`ClientError::InvalidArgs`] on
    ///    failure rather than letting the adapter see malformed input.
    /// 4. Inspect the manifest's `auth` declaration:
    ///    - `None` → call anonymously via [`call_anonymous`].
    ///    - `Oauth2` → look up an authorized connection in the wallet
    ///      and call via [`call_authed`].
    ///    - `ApiKey` → return [`ClientError::UnsupportedAuthMethod`]
    ///      (planned for v0.2).
    /// 5. Log the chosen tier via `tracing` (per the architecture's
    ///    tier-used-logging requirement).
    /// 6. Parse the response body as JSON and return it.
    ///
    /// Returns the parsed JSON response body.
    pub async fn dispatch(
        &self,
        capability: &CapabilityRef,
        intent: IntentVerb,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, ClientError> {
        // 1 + 2 + (input schema lookup): registry lookup + intent membership check + backend.
        let (backend, auth, tier, input_schema) = {
            let registry = self.registry.lock().await;
            let record = registry
                .get(capability)
                .ok_or_else(|| ClientError::CapabilityNotRegistered(capability.clone()))?;
            if !record.manifest().intents().contains(&intent) {
                return Err(ClientError::IntentNotSupported {
                    capability: capability.clone(),
                    intent,
                });
            }
            // M9 v0.3: kind check at the client boundary so callers
            // see a typed mismatch instead of an opaque transport
            // error when they use the wrong dispatch flavour.
            if record.manifest().intent_kind(&intent) == ferridis_core::IntentKind::Stream {
                return Err(ClientError::IntentRequiresStreaming {
                    capability: capability.clone(),
                    intent,
                });
            }
            let schema = record.input_schemas().and_then(|m| m.get(&intent).cloned());
            (
                record.backend().clone(),
                record.manifest().auth().clone(),
                record.manifest().tiers().preferred(),
                schema,
            )
        };

        // 3. Pre-flight validation against the declared input schema,
        // if one is known. No-op when the capability is native and the
        // manifest's capability-level OpenAPI hasn't been split into
        // per-intent schemas yet (planned for a later v0.2 pass).
        if let Some(schema) = input_schema {
            validate_dispatch_body(capability, &intent, &schema, &body)?;
        }

        match backend {
            crate::registry::CapabilityBackend::Native { base_url } => {
                self.dispatch_native(capability, intent, body, &base_url, auth, tier)
                    .await
            }
            crate::registry::CapabilityBackend::Mcp {
                client,
                intent_to_mcp,
            } => {
                let mcp_tool_name = intent_to_mcp.get(&intent).cloned().ok_or_else(|| {
                    ClientError::IntentNotSupported {
                        capability: capability.clone(),
                        intent: intent.clone(),
                    }
                })?;
                tracing::info!(
                    capability = %capability,
                    intent = %intent,
                    mcp_tool = %mcp_tool_name,
                    server = %client.server_name(),
                    tier = ?tier,
                    "dispatching MCP-backed call"
                );
                client.call_tool(&mcp_tool_name, body).await
            }
        }
    }

    /// Invoke a stream-kind `intent` on `capability` and return an
    /// ordered stream of response chunks.
    ///
    /// Companion to [`Self::dispatch`] for streamed intents (those
    /// the manifest declares as `kind: "stream"`). The wire transport
    /// is SSE: the adapter responds with `Content-Type:
    /// text/event-stream` and emits `chunk` events whose `data`
    /// payloads are the streamed values, terminated by an `end`
    /// event. An `error` event signals abnormal termination and the
    /// stream yields a final `Err(_)` before ending.
    ///
    /// Errors at the client boundary:
    /// - [`ClientError::CapabilityNotRegistered`] if `capability`
    ///   isn't in the registry.
    /// - [`ClientError::IntentNotSupported`] if the manifest does
    ///   not declare `intent`.
    /// - [`ClientError::IntentNotStreaming`] if the intent is
    ///   request-kind (use `dispatch` instead).
    /// - [`ClientError::UnsupportedAuthMethod`] for MCP-backed
    ///   capabilities (their streaming bridge is v0.4) and for
    ///   API-key auth in v0.3.
    /// - [`ClientError::InvalidArgs`] if the body fails the
    ///   declared input schema.
    ///
    /// [`Self::dispatch`], with `origin` recorded on its tier-used
    /// telemetry.
    ///
    /// The dispatch runs inside a `ferridis_dispatch` span carrying
    /// `caller = origin`, so every tier-used event it emits, on the native
    /// and the MCP path alike, names who asked for the call.
    ///
    /// # Errors
    ///
    /// As [`Self::dispatch`].
    pub async fn dispatch_from(
        &self,
        origin: CallOrigin,
        capability: &CapabilityRef,
        intent: IntentVerb,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, ClientError> {
        self.dispatch(capability, intent, body)
            .instrument(tracing::info_span!("ferridis_dispatch", caller = %origin))
            .await
    }

    /// [`Self::dispatch_streaming`], with `origin` recorded on its
    /// tier-used telemetry (see [`Self::dispatch_from`]). The span covers
    /// stream setup, where the tier-used event is emitted.
    ///
    /// # Errors
    ///
    /// As [`Self::dispatch_streaming`].
    pub async fn dispatch_streaming_from(
        &self,
        origin: CallOrigin,
        capability: &CapabilityRef,
        intent: IntentVerb,
        body: serde_json::Value,
    ) -> Result<
        std::pin::Pin<
            Box<dyn futures_util::Stream<Item = Result<serde_json::Value, ClientError>> + Send>,
        >,
        ClientError,
    > {
        self.dispatch_streaming(capability, intent, body)
            .instrument(tracing::info_span!("ferridis_dispatch", caller = %origin))
            .await
    }

    /// `auth: Oauth2` is supported for streamed intents and adds a
    /// bearer header to the SSE request.
    pub async fn dispatch_streaming(
        &self,
        capability: &CapabilityRef,
        intent: IntentVerb,
        body: serde_json::Value,
    ) -> Result<
        std::pin::Pin<
            Box<dyn futures_util::Stream<Item = Result<serde_json::Value, ClientError>> + Send>,
        >,
        ClientError,
    > {
        // Registry lookup + intent membership + kind check + schema +
        // backend selection. MCP-backed goes through a one-shot
        // tools/call path (MCP 2024-11-05 has no native streaming —
        // we surface the single response as a stream-of-one so
        // callers get a working code path regardless of backend).
        enum StreamBackend {
            Native(Url),
            Mcp {
                client: crate::mcp::McpClient,
                tool_name: String,
            },
        }
        let (backend, auth, tier, input_schema) = {
            let registry = self.registry.lock().await;
            let record = registry
                .get(capability)
                .ok_or_else(|| ClientError::CapabilityNotRegistered(capability.clone()))?;
            if !record.manifest().intents().contains(&intent) {
                return Err(ClientError::IntentNotSupported {
                    capability: capability.clone(),
                    intent,
                });
            }
            if record.manifest().intent_kind(&intent) != ferridis_core::IntentKind::Stream {
                return Err(ClientError::IntentNotStreaming {
                    capability: capability.clone(),
                    intent,
                });
            }
            let backend = match record.backend() {
                crate::registry::CapabilityBackend::Native { base_url } => {
                    StreamBackend::Native(base_url.clone())
                }
                crate::registry::CapabilityBackend::Mcp {
                    client,
                    intent_to_mcp,
                } => {
                    let tool_name = intent_to_mcp.get(&intent).cloned().ok_or_else(|| {
                        ClientError::IntentNotSupported {
                            capability: capability.clone(),
                            intent: intent.clone(),
                        }
                    })?;
                    StreamBackend::Mcp {
                        client: client.clone(),
                        tool_name,
                    }
                }
            };
            let schema = record.input_schemas().and_then(|m| m.get(&intent).cloned());
            (
                backend,
                record.manifest().auth().clone(),
                record.manifest().tiers().preferred(),
                schema,
            )
        };

        // Pre-flight input validation if we have a schema.
        if let Some(schema) = input_schema {
            validate_dispatch_body(capability, &intent, &schema, &body)?;
        }

        // MCP-backed streaming: MCP 2024-11-05's `tools/call` is
        // request/response; there's no progressive-stream semantics
        // in the spec yet. We honour the dispatch_streaming contract
        // by wrapping the single response in a stream-of-one. When
        // the MCP spec adds streaming (notifications-as-chunks is the
        // candidate shape), this is where the upgrade lands.
        if let StreamBackend::Mcp { client, tool_name } = &backend {
            tracing::info!(
                capability = %capability,
                intent = %intent,
                mcp_tool = %tool_name,
                server = %client.server_name(),
                tier = ?tier,
                "dispatching MCP-backed streaming call (one-shot bridge)"
            );
            let response = client.call_tool(tool_name, body).await?;
            let one_shot = async_stream::stream! {
                yield Ok(response);
            };
            return Ok(Box::pin(one_shot)
                as std::pin::Pin<
                    Box<
                        dyn futures_util::Stream<Item = Result<serde_json::Value, ClientError>>
                            + Send,
                    >,
                >);
        }

        // Native path: actually stream over SSE.
        let base_url = match backend {
            StreamBackend::Native(u) => u,
            StreamBackend::Mcp { .. } => unreachable!("handled above"),
        };

        // Build the SSE request. POST the body and accept
        // text/event-stream; the adapter is expected to respond with
        // a streaming body.
        let url = build_intent_url(&base_url, &intent)?;
        let mut req = self
            .http
            .http()
            .post(url.clone())
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body);
        match auth {
            AuthMethod::None => {
                tracing::info!(
                    capability = %capability,
                    intent = %intent,
                    tier = ?tier,
                    auth = "none",
                    "dispatching streaming call"
                );
            }
            AuthMethod::Oauth2 { .. } => {
                let conn = self
                    .wallet
                    .lock()
                    .await
                    .authorized(capability)
                    .ok_or_else(|| ClientError::NoAuthorizedConnection(capability.clone()))?;
                tracing::info!(
                    capability = %capability,
                    intent = %intent,
                    tier = ?tier,
                    auth = "oauth2",
                    connection_id = %conn.id(),
                    "dispatching authenticated streaming call"
                );
                req = req.bearer_auth(conn.access_token().expose());
            }
            AuthMethod::ApiKey { .. } => {
                return Err(ClientError::UnsupportedAuthMethod("api_key"));
            }
        }
        let resp = req.send().await.map_err(|e| {
            ClientError::from(ferridis_protocol::ProtocolError::Transport {
                url: url.clone(),
                source: e,
            })
        })?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ClientError::from(
                ferridis_protocol::ProtocolError::BadStatus {
                    url,
                    status: status.as_u16(),
                    body: body.chars().take(512).collect(),
                },
            ));
        }

        // Pipe the bytes through the existing SSE parser and re-shape
        // the events into a stream of `Result<Value, ClientError>`:
        //
        //   event: chunk        → yield Ok(data)
        //   event: end          → end the stream
        //   event: error        → yield one Err(_) then end
        //   event: backpressure → honor the flow-control signal:
        //                         slow-down is advisory (logged);
        //                         halt yields Err(StreamHalted) and ends
        //   (anything else: silently skipped per the SSE spec)
        use futures_util::StreamExt;
        let stream_url = url.clone();
        let raw_bytes = Box::pin(resp.bytes_stream().map(move |chunk| {
            chunk.map_err(|source| ferridis_protocol::ProtocolError::Transport {
                url: stream_url.clone(),
                source,
            })
        }));
        let events = ferridis_protocol::parse_sse_stream(raw_bytes);
        let cap_for_err = capability.clone();
        let intent_for_err = intent.clone();
        let mapped = async_stream::stream! {
            let mut events = Box::pin(events);
            while let Some(item) = futures_util::StreamExt::next(&mut events).await {
                match item {
                    Ok(ev) => match ev.name.as_str() {
                        "chunk" => yield Ok(ev.data),
                        "end" => break,
                        "error" => {
                            // The adapter signalled abnormal termination.
                            // Surface the data as a typed dispatch error
                            // and stop. v0.4 will define the canonical
                            // error envelope shape.
                            let details = match &ev.data {
                                serde_json::Value::String(s) => s.clone(),
                                other => other.to_string(),
                            };
                            yield Err(ClientError::InvalidArgs {
                                capability: cap_for_err.clone(), // clone: error owns the ref; the stream closure keeps its copy
                                intent: intent_for_err.clone(), // clone: error owns the verb; the stream closure keeps its copy
                                details: format!("adapter signalled stream error: {details}"),
                            });
                            break;
                        }
                        "backpressure" => {
                            use serde::Deserialize as _;
                            let signal = ev
                                .data
                                .get("signal")
                                .and_then(|v| {
                                    ferridis_protocol::BackpressureSignal::deserialize(v).ok()
                                })
                                .unwrap_or_default();
                            match signal {
                                ferridis_protocol::BackpressureSignal::Halt => {
                                    // Deliberate flow-control stop: the
                                    // adapter is overwhelmed and has
                                    // terminated the stream.
                                    yield Err(ClientError::StreamHalted {
                                        capability: cap_for_err.clone(), // clone: error owns the ref; the stream closure keeps its copy
                                        intent: intent_for_err.clone(), // clone: error owns the verb; the stream closure keeps its copy
                                    });
                                    break;
                                }
                                ferridis_protocol::BackpressureSignal::SlowDown => {
                                    // Advisory only in this API: surface
                                    // it to operators; a rate-limiting
                                    // consumer belongs at the caller.
                                    tracing::debug!(
                                        capability = %cap_for_err,
                                        intent = %intent_for_err,
                                        "adapter signalled slow-down on streamed intent"
                                    );
                                }
                                ferridis_protocol::BackpressureSignal::Continue => {}
                            }
                        }
                        _ => {
                            // Unknown event name — per SSE convention,
                            // ignore.
                        }
                    },
                    Err(e) => {
                        yield Err(ClientError::from(e));
                        break;
                    }
                }
            }
        };
        Ok(Box::pin(mapped))
    }

    async fn dispatch_native(
        &self,
        capability: &CapabilityRef,
        intent: IntentVerb,
        body: serde_json::Value,
        base_url: &Url,
        auth: AuthMethod,
        tier: ferridis_core::Tier,
    ) -> Result<serde_json::Value, ClientError> {
        let url = build_intent_url(base_url, &intent)?;
        let request = CallRequest::new(Method::POST, url, intent.clone()).with_json_body(body);

        let response: CallResponse = match auth {
            AuthMethod::None => {
                tracing::info!(
                    capability = %capability,
                    intent = %intent,
                    tier = ?tier,
                    auth = "none",
                    "dispatching anonymous call"
                );
                call_anonymous(&self.http, request).await?
            }
            AuthMethod::Oauth2 { .. } => {
                let conn = self
                    .wallet
                    .lock()
                    .await
                    .authorized(capability)
                    .ok_or_else(|| ClientError::NoAuthorizedConnection(capability.clone()))?;
                tracing::info!(
                    capability = %capability,
                    intent = %intent,
                    tier = ?tier,
                    auth = "oauth2",
                    connection_id = %conn.id(),
                    "dispatching authenticated call"
                );
                call_authed(&self.http, &conn, request).await?
            }
            AuthMethod::ApiKey { .. } => {
                return Err(ClientError::UnsupportedAuthMethod("api_key"));
            }
        };

        let parsed: serde_json::Value =
            serde_json::from_slice(response.body()).map_err(ClientError::from)?;
        Ok(parsed)
    }

    /// Register an MCP server consumed over stdio as a Tier::Native
    /// capability. Spawns the server, performs the MCP handshake,
    /// fetches its `tools/list`, projects each tool into a Ferridis
    /// intent verb, and registers the synthetic manifest with the
    /// personal-tier registry.
    ///
    /// Returns the [`CapabilityRef`] under which the MCP server now
    /// appears. Use [`Self::dispatch`] with that capability + a
    /// projected intent verb to invoke the underlying MCP tool.
    pub async fn register_mcp_stdio(
        &self,
        command: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<ferridis_core::CapabilityRef, ClientError> {
        let transport = crate::mcp::StdioTransport::spawn(command, args, env).await?;
        let (mcp_client, tools) =
            crate::mcp::McpClient::handshake(transport, "ferridis-client").await?;
        let projected =
            crate::mcp::project_tools_to_manifest(mcp_client.server_name(), "mcp", "v1", &tools)?;
        let capability = projected.capability.clone();
        let record = crate::registry::CapabilityRecord::with_backend(
            capability.clone(),
            projected.manifest,
            crate::registry::CapabilityBackend::Mcp {
                client: mcp_client,
                intent_to_mcp: std::sync::Arc::new(projected.intent_to_mcp),
            },
        )
        .with_input_schemas(projected.intent_to_input_schema);
        self.registry.lock().await.insert(record);
        tracing::info!(
            capability = %capability,
            tools = tools.len(),
            "registered MCP server (stdio) with personal-tier registry"
        );
        Ok(capability)
    }

    /// Register an MCP server consumed over SSE as a Tier::Native
    /// capability. Otherwise identical to [`Self::register_mcp_stdio`].
    pub async fn register_mcp_sse(
        &self,
        sse_url: Url,
    ) -> Result<ferridis_core::CapabilityRef, ClientError> {
        let transport = crate::mcp::SseTransport::open(sse_url.clone()).await?;
        transport.wait_for_endpoint(&sse_url).await?;
        transport.bind_origin(&sse_url).await?;
        let (mcp_client, tools) =
            crate::mcp::McpClient::handshake(transport, "ferridis-client").await?;
        let projected =
            crate::mcp::project_tools_to_manifest(mcp_client.server_name(), "mcp", "v1", &tools)?;
        let capability = projected.capability.clone();
        let record = crate::registry::CapabilityRecord::with_backend(
            capability.clone(),
            projected.manifest,
            crate::registry::CapabilityBackend::Mcp {
                client: mcp_client,
                intent_to_mcp: std::sync::Arc::new(projected.intent_to_mcp),
            },
        )
        .with_input_schemas(projected.intent_to_input_schema);
        self.registry.lock().await.insert(record);
        tracing::info!(
            capability = %capability,
            sse_url = %sse_url,
            tools = tools.len(),
            "registered MCP server (SSE) with personal-tier registry"
        );
        Ok(capability)
    }

    /// Register a capability discovered via the public mesh.
    ///
    /// The flow:
    ///
    /// 1. Fetch the manifest body and its cosign bundle from the
    ///    mesh at the convention URL (see
    ///    [`ferridis_protocol::mesh`] for the URL shape).
    /// 2. Verify the bundle's signature, the cert validity window,
    ///    and (when v0.3 lands) the Fulcio chain + Rekor inclusion.
    /// 3. Parse the verified manifest bytes through
    ///    [`ferridis_core::Manifest::parse`].
    /// 4. Determine the dispatch base URL — preferring the
    ///    manifest's `endpoint_url`, falling back to
    ///    `fallback_base_url` if the manifest does not declare one.
    ///    Returns [`ClientError::InvalidUrl`] when both are absent.
    /// 5. Register the capability in the [`Registry`](crate::Registry)
    ///    and **retag the record as [`RegistryTier::Public`]** so
    ///    `candidates_for_intent` orders mesh-discovered options
    ///    below personal and org-pushed ones.
    ///
    /// `fallback_base_url` covers v0.2 transitional manifests that
    /// don't yet carry `endpoint_url`. v0.3 makes the field required
    /// and removes the fallback.
    pub async fn register_from_mesh(
        &self,
        capability: CapabilityRef,
        mesh_root: Url,
        fallback_base_url: Option<Url>,
    ) -> Result<Manifest, ClientError> {
        let mesh = MeshClient::new(self.http.clone(), mesh_root.clone());
        let artifact = mesh.fetch_and_verify(&capability).await?;
        let manifest = Manifest::parse(
            std::str::from_utf8(&artifact.manifest_bytes)
                .map_err(|e| ClientError::WalletParse(format!("mesh manifest not UTF-8: {e}")))?,
        )?;
        let base_url = manifest
            .endpoint_url()
            .cloned()
            .or(fallback_base_url)
            .ok_or_else(|| {
                ClientError::InvalidUrl(format!(
                    "mesh manifest for {capability} declares no endpoint_url and no fallback was provided"
                ))
            })?;
        let endpoint_source = if manifest.endpoint_url().is_some() {
            "manifest"
        } else {
            "fallback"
        };
        let record =
            crate::registry::CapabilityRecord::new(capability.clone(), manifest.clone(), base_url);
        {
            let mut registry = self.registry.lock().await;
            registry.insert(record);
            // The Personal default is overwritten in place.
            registry
                .set_tier(&capability, crate::registry::RegistryTier::Public)
                .expect("just-inserted record must be retaggable");
        }
        tracing::info!(
            capability = %capability,
            mesh_root = %mesh_root,
            endpoint_source,
            "registered mesh-discovered capability with public-tier registry"
        );
        Ok(manifest)
    }

    /// Register a capability through a `FederatedMesh` chain.
    ///
    /// Tries each mesh in the chain (priority order: index 0 is
    /// highest), falling through on 404 until one tier has the cap
    /// or every tier 404s (`ProtocolError::CapabilityNotFound`).
    /// Identical to [`Self::register_from_mesh`] otherwise: parses
    /// the verified manifest, resolves the dispatch base URL
    /// (manifest's `endpoint_url` if present, else `fallback`),
    /// registers as `RegistryTier::Public`.
    pub async fn register_from_federated_mesh(
        &self,
        capability: CapabilityRef,
        federation: &FederatedMesh,
        fallback_base_url: Option<Url>,
    ) -> Result<Manifest, ClientError> {
        let artifact = federation.fetch_and_verify(&capability).await?;
        let manifest =
            Manifest::parse(std::str::from_utf8(&artifact.manifest_bytes).map_err(|e| {
                ClientError::WalletParse(format!("federated mesh manifest not UTF-8: {e}"))
            })?)?;
        let base_url = manifest
            .endpoint_url()
            .cloned()
            .or(fallback_base_url)
            .ok_or_else(|| {
                ClientError::InvalidUrl(format!(
                    "mesh manifest for {capability} declares no endpoint_url and no fallback was provided"
                ))
            })?;
        let endpoint_source = if manifest.endpoint_url().is_some() {
            "manifest"
        } else {
            "fallback"
        };
        let record =
            crate::registry::CapabilityRecord::new(capability.clone(), manifest.clone(), base_url);
        {
            let mut registry = self.registry.lock().await;
            registry.insert(record);
            registry
                .set_tier(&capability, crate::registry::RegistryTier::Public)
                .expect("just-inserted record must be retaggable");
        }
        tracing::info!(
            capability = %capability,
            federation_size = federation.meshes().len(),
            endpoint_source,
            "registered mesh-discovered capability with public-tier registry (federated)"
        );
        Ok(manifest)
    }

    /// Auto-populate the Public tier from a mesh's signed index.
    ///
    /// Fetches `{mesh_root}/index.json` + cosign bundle, verifies
    /// through `trust_root` (full Sigstore chain), then registers
    /// every entry in the index as a Public-tier capability. The
    /// expected deployment pattern is: a host runs this once at
    /// startup to discover the mesh's full capability surface, then
    /// uses `candidates_for_intent` to route at dispatch time.
    ///
    /// Returns the list of successfully-registered capability refs;
    /// failures (one entry whose manifest is unreachable, signature
    /// is bad, etc.) are logged at WARN and skipped — one bad entry
    /// shouldn't tank the whole index load. Hosts that want strict
    /// all-or-nothing behaviour can iterate entries themselves via
    /// `mesh.fetch_index_and_verify(...)` + `register_from_mesh`.
    pub async fn register_all_from_mesh(
        &self,
        mesh: &MeshClient,
        trust_root: &TrustRoot,
        fallback_base_url: Option<Url>,
    ) -> Result<Vec<CapabilityRef>, ClientError> {
        let index = mesh
            .fetch_index_and_verify(trust_root)
            .await
            .map_err(ClientError::from)?;
        let mut registered = Vec::with_capacity(index.len());
        for entry in &index.entries {
            match self
                .register_from_mesh(
                    entry.capability.clone(),
                    mesh.mesh_root().clone(),
                    fallback_base_url.clone(),
                )
                .await
            {
                Ok(_) => registered.push(entry.capability.clone()),
                Err(e) => {
                    tracing::warn!(
                        capability = %entry.capability,
                        mesh_root = %mesh.mesh_root(),
                        error = %e,
                        "skipping mesh-index entry that failed to register"
                    );
                }
            }
        }
        tracing::info!(
            mesh_root = %mesh.mesh_root(),
            index_entries = index.len(),
            registered = registered.len(),
            "auto-populated Public tier from signed mesh index"
        );
        Ok(registered)
    }

    /// Subscribe to a named server-sent-event channel on a registered
    /// capability. Returns a stream of [`ServerEvent`]s.
    ///
    /// v0.2 scope is **server → client only** over SSE. The channel
    /// URL is built by convention as `{base_url}/events/{channel}`
    /// (mirroring `{base_url}/intents/{verb}` for calls). Manifests
    /// do not yet declare event channels by name; v0.3 will add a
    /// typed `events` field to the manifest and a WebSocket bidi
    /// transport.
    ///
    /// Limitations and current behaviour:
    ///
    /// - Native (HTTP-backed) capabilities only. MCP-backed
    ///   capabilities return [`ClientError::UnsupportedAuthMethod`]
    ///   for now — MCP has its own notifications channel that needs a
    ///   different bridge.
    /// - The subscription is unauthenticated by default (treated like
    ///   anonymous dispatch). v0.3 will plumb bearer tokens through
    ///   the SSE request when the manifest requires `Oauth2`.
    /// - The returned stream lives until the server closes the
    ///   connection or the caller drops the stream. Auto-reconnect is
    ///   a v0.3 concern.
    pub async fn subscribe(
        &self,
        capability: &CapabilityRef,
        channel: &str,
    ) -> Result<impl Stream<Item = Result<ServerEvent, ClientError>>, ClientError> {
        let base_url = {
            let registry = self.registry.lock().await;
            let record = registry
                .get(capability)
                .ok_or_else(|| ClientError::CapabilityNotRegistered(capability.clone()))?;
            // M6 v0.3: if the manifest declares any event_channels,
            // the channel name must match one of them. If the
            // manifest declares none, allow any channel (capability
            // hasn't been updated to v0.3 manifest format yet — a
            // legacy fallback that goes away when `event_channels`
            // becomes required).
            let channels = record.manifest().event_channels();
            if !channels.is_empty() && !record.manifest().has_event_channel(channel) {
                let declared = channels.iter().map(|c| c.name().to_owned()).collect();
                return Err(ClientError::EventChannelNotDeclared {
                    capability: capability.clone(),
                    channel: channel.to_string(),
                    declared,
                });
            }
            match record.backend() {
                crate::registry::CapabilityBackend::Native { base_url } => base_url.clone(),
                crate::registry::CapabilityBackend::Mcp { .. } => {
                    return Err(ClientError::UnsupportedAuthMethod(
                        "MCP-backed event subscription (v0.3)",
                    ));
                }
            }
        };

        let url = build_events_url(&base_url, channel)?;
        let events = protocol_subscribe(&self.http, &url).await?;
        tracing::info!(
            capability = %capability,
            channel = %channel,
            url = %url,
            "subscribed to event channel"
        );
        Ok(futures_util::StreamExt::map(events, |item| {
            item.map_err(ClientError::from)
        }))
    }

    /// Open a WebSocket connection to a named event channel.
    ///
    /// The channel must be declared in the manifest with
    /// `transport: "websocket"`. Channels declared as `"sse"` (or
    /// with no `transport` field, which defaults to SSE) return
    /// [`ClientError::WrongTransport`]; use [`subscribe`](Self::subscribe)
    /// for those.
    ///
    /// Returns a [`WsConnection`] the caller can split into independent
    /// send/receive halves or use directly via `send`/`next`.
    pub async fn subscribe_ws(
        &self,
        capability: &CapabilityRef,
        channel: &str,
    ) -> Result<WsConnection, ClientError> {
        let (base_url, auth) = {
            let registry = self.registry.lock().await;
            let record = registry
                .get(capability)
                .ok_or_else(|| ClientError::CapabilityNotRegistered(capability.clone()))?; // clone: error needs owned value, capability is borrowed from caller

            let channels = record.manifest().event_channels();
            if !channels.is_empty() {
                if !record.manifest().has_event_channel(channel) {
                    let declared = channels.iter().map(|c| c.name().to_owned()).collect();
                    return Err(ClientError::EventChannelNotDeclared {
                        capability: capability.clone(), // clone: error needs owned value
                        channel: channel.to_string(),
                        declared,
                    });
                }
                if let Some(transport) = record.manifest().channel_transport(channel)
                    && transport != ferridis_core::ChannelTransport::WebSocket
                {
                    return Err(ClientError::WrongTransport {
                        channel: channel.to_string(),
                        declared: transport,
                        requested: ferridis_core::ChannelTransport::WebSocket,
                    });
                }
            }

            let base_url = match record.backend() {
                crate::registry::CapabilityBackend::Native { base_url } => base_url.clone(), // clone: base_url is behind a registry lock, must be freed before await
                crate::registry::CapabilityBackend::Mcp { .. } => {
                    return Err(ClientError::UnsupportedAuthMethod(
                        "MCP-backed WebSocket subscription",
                    ));
                }
            };

            let auth = record.manifest().auth().clone(); // clone: manifest is behind registry lock, freed before await
            (base_url, auth)
        };

        let bearer = match auth {
            ferridis_core::AuthMethod::None => None,
            ferridis_core::AuthMethod::Oauth2 { .. } => {
                let conn = self
                    .wallet
                    .lock()
                    .await
                    .authorized(capability)
                    .ok_or_else(|| ClientError::NoAuthorizedConnection(capability.clone()))?; // clone: error needs owned value
                Some(conn.access_token().expose().to_string())
            }
            ferridis_core::AuthMethod::ApiKey { .. } => {
                return Err(ClientError::UnsupportedAuthMethod(
                    "ApiKey auth for WebSocket",
                ));
            }
        };
        let ws_url = http_url_to_ws(&base_url, channel)?;
        tracing::info!(
            capability = %capability,
            channel = %channel,
            url = %ws_url,
            "opening WebSocket channel"
        );
        connect_ws(&ws_url, bearer.as_deref())
            .await
            .map_err(ClientError::from)
    }
}

/// Pre-flight check the outbound `body` against the capability's
/// declared JSON Schema for `intent`. Compiles the schema fresh per
/// call — adequate for v0.2; a per-capability `Validator` cache is
/// a later optimization.
///
/// If the schema itself is malformed (a bug in whatever produced it),
/// we err on the side of letting the dispatch proceed: a misshapen
/// schema should not stand between the user and a working tool. The
/// adapter's own validation will catch a truly bad body. The compile
/// failure is logged at WARN so it doesn't go silently unnoticed.
fn validate_dispatch_body(
    capability: &CapabilityRef,
    intent: &IntentVerb,
    schema: &serde_json::Value,
    body: &serde_json::Value,
) -> Result<(), ClientError> {
    let validator = match jsonschema::validator_for(schema) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                capability = %capability,
                intent = %intent,
                error = %e,
                "input schema failed to compile; skipping pre-flight validation"
            );
            return Ok(());
        }
    };
    let errors: Vec<String> = validator
        .iter_errors(body)
        .map(|e| format!("{}: {}", e.instance_path, e))
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ClientError::InvalidArgs {
            capability: capability.clone(),
            intent: intent.clone(),
            details: errors.join("; "),
        })
    }
}

/// Build the URL for a named event channel under the capability's
/// base URL.
///
/// Convention: `{base_url}/events/{channel}`. Mirrors
/// [`build_intent_url`]'s pattern for `/intents/{verb}`. v0.3 will
/// replace this convention with a manifest-declared `events_url`
/// per channel.
fn build_events_url(base_url: &Url, channel: &str) -> Result<Url, ClientError> {
    let base = if base_url.path().ends_with('/') {
        base_url.clone()
    } else {
        let mut b = base_url.clone();
        b.set_path(&format!("{}/", b.path()));
        b
    };
    base.join(&format!("events/{channel}"))
        .map_err(|e| ClientError::InvalidUrl(format!("{base_url}: {e}")))
}

/// Build a WebSocket URL for `{base_url}/events/{channel}`, converting
/// the scheme from `http`→`ws` or `https`→`wss`.
fn http_url_to_ws(base_url: &Url, channel: &str) -> Result<Url, ClientError> {
    let mut url = build_events_url(base_url, channel)?;
    let ws_scheme = match url.scheme() {
        "http" => "ws",
        "https" => "wss",
        other => {
            return Err(ClientError::InvalidUrl(format!(
                "cannot convert scheme `{other}` to WebSocket"
            )));
        }
    };
    url.set_scheme(ws_scheme)
        .map_err(|()| ClientError::InvalidUrl(format!("failed to set scheme on {url}")))?;
    Ok(url)
}

/// Build the URL for a given intent under the capability's base URL.
///
/// Convention: `{base_url}/intents/{verb}`. Trailing slashes on
/// `base_url` are handled by [`Url::join`].
fn build_intent_url(base_url: &Url, intent: &IntentVerb) -> Result<Url, ClientError> {
    base_relative_url(base_url, &format!("intents/{}", intent.as_str()))
}

/// Join `relative` under `base_url`, treating the base as a directory
/// whether or not it ends in `/`.
fn base_relative_url(base_url: &Url, relative: &str) -> Result<Url, ClientError> {
    // Ensure the base URL has a trailing slash so `Url::join` appends
    // rather than replacing the final segment.
    let base = if base_url.path().ends_with('/') {
        base_url.clone() // allow:clone — join needs an owned base in both branches; one URL per call
    } else {
        let mut b = base_url.clone(); // allow:clone — mutated to add the trailing slash; the caller's URL stays untouched
        b.set_path(&format!("{}/", b.path()));
        b
    };
    base.join(relative)
        .map_err(|e| ClientError::InvalidUrl(format!("{base_url}: {e}")))
}

/// Who asked for a dispatch. Recorded on the tier-used telemetry by
/// [`Client::dispatch_from`] and [`Client::dispatch_streaming_from`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CallOrigin {
    /// The embedding host itself, not a model.
    Host,
    /// A model calling the tool directly.
    ModelDirect,
    /// Code a model runs in its code-execution container (programmatic
    /// tool calling).
    ModelCode,
}

impl std::fmt::Display for CallOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Host => "host",
            Self::ModelDirect => "model-direct",
            Self::ModelCode => "model-code",
        })
    }
}

/// What [`Client::load_response_schemas`] found.
#[must_use]
#[derive(Debug)]
pub enum ResponseSchemaLoad {
    /// A document parsed, and its schemas are now on the registry record.
    /// `intents` can be zero: the document parsed but declares no JSON
    /// response schema.
    Loaded {
        /// The document's URL.
        from: Url,
        /// How many intents gained a response schema.
        intents: usize,
    },
    /// Every candidate failed. The record is unchanged.
    NoneFound {
        /// One entry per candidate, in the order tried.
        attempts: Vec<SchemaAttempt>,
    },
    /// MCP-backed capability. There is no OpenAPI document, and MCP
    /// 2024-11-05 carries no `outputSchema`.
    NotApplicable,
}

/// One failed candidate in [`ResponseSchemaLoad::NoneFound`].
#[derive(Debug)]
pub struct SchemaAttempt {
    url: Url,
    failure: SchemaFetchFailure,
}

impl SchemaAttempt {
    /// The candidate URL.
    #[must_use]
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Why it failed.
    #[must_use]
    pub fn failure(&self) -> &SchemaFetchFailure {
        &self.failure
    }
}

/// Why one response-schema candidate failed.
#[derive(Debug, thiserror::Error)]
pub enum SchemaFetchFailure {
    /// The document could not be fetched.
    #[error("fetch failed: {0}")]
    Fetch(#[source] Box<ferridis_protocol::ProtocolError>),
    /// The document was fetched but is not an OpenAPI 3 JSON document.
    #[error("not readable: {0}")]
    Parse(#[source] OpenApiError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_url_appends_to_root_base() {
        let base = Url::parse("http://127.0.0.1:8080").unwrap();
        let intent = IntentVerb::parse("read-file").unwrap();
        let url = build_intent_url(&base, &intent).unwrap();
        assert_eq!(url.as_str(), "http://127.0.0.1:8080/intents/read-file");
    }

    #[test]
    fn intent_url_appends_under_subpath() {
        let base = Url::parse("https://example.invalid/fs").unwrap();
        let intent = IntentVerb::parse("write-file").unwrap();
        let url = build_intent_url(&base, &intent).unwrap();
        assert_eq!(
            url.as_str(),
            "https://example.invalid/fs/intents/write-file"
        );
    }

    fn cap(seg: &str) -> CapabilityRef {
        CapabilityRef::parse(&format!("ferridis://public.ferridis.io/test/{seg}@v1")).unwrap()
    }

    /// The exact shape that motivated the validation: an integer-bounded
    /// `limit` plus typed string fields, like real linux-health-mcp.
    fn journal_query_schema() -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "priority": { "type": "string" },
                "unit":     { "type": "string" },
                "since":    { "type": "string" },
                "limit":    { "type": "integer", "minimum": 1, "maximum": 1000 }
            },
            "additionalProperties": false
        })
    }

    #[test]
    fn validate_dispatch_body_accepts_valid_args() {
        let intent = IntentVerb::parse("journal-query").unwrap();
        let body = serde_json::json!({ "limit": 5, "unit": "ssh.service" });
        validate_dispatch_body(&cap("a"), &intent, &journal_query_schema(), &body).unwrap();
    }

    #[test]
    fn validate_dispatch_body_rejects_wrong_type() {
        // Integer expected, string given — mirrors the Claude-Code bug
        // where `limit: 5` got stringified to `"5"` and the stdio
        // child rejected it. Pre-flight catches this now.
        let intent = IntentVerb::parse("journal-query").unwrap();
        let body = serde_json::json!({ "limit": "5" });
        let err =
            validate_dispatch_body(&cap("a"), &intent, &journal_query_schema(), &body).unwrap_err();
        match err {
            ClientError::InvalidArgs {
                intent: i, details, ..
            } => {
                assert_eq!(i.as_str(), "journal-query");
                assert!(
                    details.to_lowercase().contains("integer")
                        || details.to_lowercase().contains("type"),
                    "expected a type-mismatch hint, got: {details}"
                );
            }
            other => panic!("expected InvalidArgs, got {other:?}"),
        }
    }

    #[test]
    fn validate_dispatch_body_rejects_out_of_bounds() {
        let intent = IntentVerb::parse("journal-query").unwrap();
        let body = serde_json::json!({ "limit": 99999 });
        assert!(
            validate_dispatch_body(&cap("a"), &intent, &journal_query_schema(), &body).is_err(),
            "limit=99999 exceeds schema maximum"
        );
    }

    #[test]
    fn validate_dispatch_body_rejects_additional_properties() {
        let intent = IntentVerb::parse("journal-query").unwrap();
        let body = serde_json::json!({ "unit": "ssh.service", "extra_field": "x" });
        assert!(
            validate_dispatch_body(&cap("a"), &intent, &journal_query_schema(), &body).is_err(),
            "additionalProperties:false should reject extra_field"
        );
    }

    #[test]
    fn validate_dispatch_body_is_lenient_on_broken_schema() {
        // A schema that fails to compile (`type` is not a string or
        // array of strings) should not block dispatch — the adapter's
        // own validation remains as a safety net.
        let intent = IntentVerb::parse("foo").unwrap();
        let bad_schema = serde_json::json!({ "type": 42 });
        let body = serde_json::json!({ "anything": true });
        validate_dispatch_body(&cap("a"), &intent, &bad_schema, &body).unwrap();
    }
}
