//! MCP server runtime: stdio loop and per-method handlers.

use std::sync::Arc;

use ferridis_client::Client;
use ferridis_client::catalogue::{CatalogueEntry, collect_stream};
use ferridis_core::IntentKind;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tracing::{error, info, warn};

use crate::protocol::{
    ContentPart, Inbound, InitializeResult, PROTOCOL_VERSION, Response, SERVER_NAME,
    SERVER_VERSION, ServerCapabilities, ServerInfo, Tool, ToolsCallParams, ToolsCallResult,
    ToolsCapability, ToolsListResult, codes,
};
use crate::ptc_tool::{PTC_TOOL_NAME, PtcTool};
use crate::tools::ToolCatalogue;

/// Long-lived server state.
pub struct Server {
    client: Arc<Client>,
    catalogue: ToolCatalogue,
    /// `ferridis_ptc_run`, when `--ptc-api-key-file` configured it.
    ptc: Option<PtcTool>,
    /// Set when the client sends `initialize`.
    initialized: std::sync::Mutex<bool>,
}

impl Server {
    pub fn new(client: Arc<Client>, catalogue: ToolCatalogue, ptc: Option<PtcTool>) -> Self {
        Self {
            client,
            catalogue,
            ptc,
            initialized: std::sync::Mutex::new(false),
        }
    }

    /// Drive the stdio loop until EOF.
    pub async fn serve_stdio(self: Arc<Self>) {
        let stdin = tokio::io::stdin();
        let stdout = tokio::io::stdout();
        let mut reader = BufReader::new(stdin).lines();
        let mut writer = BufWriter::new(stdout);

        loop {
            let line = match reader.next_line().await {
                Ok(Some(l)) => l,
                Ok(None) => {
                    info!("stdin closed, exiting");
                    break;
                }
                Err(e) => {
                    error!(error = %e, "stdin read failed");
                    break;
                }
            };

            if line.trim().is_empty() {
                continue;
            }

            let parsed: Result<Inbound, _> = serde_json::from_str(&line);
            match parsed {
                Ok(msg) if !msg.is_valid() => {
                    let id = msg.id.clone().unwrap_or(serde_json::Value::Null);
                    let response = Response::error(
                        id,
                        codes::INVALID_REQUEST,
                        "jsonrpc field must be \"2.0\"",
                    );
                    if let Err(e) = write_response(&mut writer, &response).await {
                        error!(error = %e, "stdout write failed");
                        break;
                    }
                }
                Ok(msg) if msg.is_notification() => {
                    self.handle_notification(&msg).await;
                }
                Ok(msg) => {
                    let response = self.handle_request(msg).await;
                    if let Err(e) = write_response(&mut writer, &response).await {
                        error!(error = %e, "stdout write failed");
                        break;
                    }
                }
                Err(e) => {
                    let response = Response::error(
                        serde_json::Value::Null,
                        codes::PARSE_ERROR,
                        format!("parse error: {e}"),
                    );
                    if let Err(e) = write_response(&mut writer, &response).await {
                        error!(error = %e, "stdout write failed");
                        break;
                    }
                }
            }
        }
    }

    /// Dispatch one parsed notification. Public to the crate so
    /// alternate transports (e.g., the HTTP transport) can route to
    /// the same handler the stdio loop uses.
    pub(crate) async fn handle_notification(&self, msg: &Inbound) {
        match msg.method.as_str() {
            "notifications/initialized" => {
                *self.initialized.lock().expect("mutex") = true;
                info!("client signaled initialized");
            }
            other => {
                warn!(method = %other, "ignoring unknown notification");
            }
        }
    }

    /// Dispatch one parsed request and return the response. Public to
    /// the crate so alternate transports (e.g., the HTTP transport)
    /// can route to the same handler the stdio loop uses.
    pub(crate) async fn handle_request(&self, msg: Inbound) -> Response {
        let id = msg.id.clone().unwrap_or(serde_json::Value::Null);
        match msg.method.as_str() {
            "initialize" => self.handle_initialize(id).await,
            "ping" => Response::success(id, json!({})),
            "tools/list" => self.handle_tools_list(id).await,
            "tools/call" => self.handle_tools_call(id, msg.params).await,
            other => {
                warn!(method = %other, "method not found");
                Response::error(
                    id,
                    codes::METHOD_NOT_FOUND,
                    format!("unknown method: {other}"),
                )
            }
        }
    }

    async fn handle_initialize(&self, id: serde_json::Value) -> Response {
        let result = InitializeResult {
            protocol_version: PROTOCOL_VERSION,
            capabilities: ServerCapabilities {
                tools: ToolsCapability::default(),
            },
            server_info: ServerInfo {
                name: SERVER_NAME,
                version: SERVER_VERSION,
            },
        };
        Response::success(
            id,
            serde_json::to_value(result).expect("InitializeResult serializes"),
        )
    }

    async fn handle_tools_list(&self, id: serde_json::Value) -> Response {
        let mut tools: Vec<Tool> = self.catalogue.tools().to_vec();
        if let Some(ptc) = &self.ptc {
            tools.push(ptc.descriptor().clone()); // allow:clone — the listing is serialized and dropped; the tool keeps its descriptor
        }
        let result = ToolsListResult { tools };
        Response::success(
            id,
            serde_json::to_value(result).expect("ToolsListResult serializes"),
        )
    }

    async fn handle_tools_call(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
    ) -> Response {
        let call: ToolsCallParams = match serde_json::from_value(params) {
            Ok(v) => v,
            Err(e) => {
                return Response::error(
                    id,
                    codes::INVALID_PARAMS,
                    format!("invalid tools/call params: {e}"),
                );
            }
        };

        if let Some(ptc) = self.ptc.as_ref().filter(|_| call.name == PTC_TOOL_NAME) {
            info!(tool = %call.name, "tools/call programmatic run");
            let result = ptc.call(call.arguments).await;
            return Response::success(
                id,
                serde_json::to_value(result).expect("ToolsCallResult serializes"),
            );
        }

        let Some(entry) = self.catalogue.resolve(&call.name) else {
            return Response::error(
                id,
                codes::METHOD_NOT_FOUND,
                format!("unknown tool: {}", call.name),
            );
        };

        info!(
            tool = %call.name,
            capability = %entry.capability(),
            intent = %entry.intent(),
            "tools/call dispatch"
        );

        match self.dispatch(entry, call.arguments).await {
            Ok(body) => {
                let text = serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string());
                let result = ToolsCallResult {
                    content: vec![ContentPart::Text { text }],
                    is_error: None,
                };
                Response::success(
                    id,
                    serde_json::to_value(result).expect("ToolsCallResult serializes"),
                )
            }
            Err(e) => {
                // MCP convention: surface dispatch failures as a `tools/call`
                // result with `isError: true` and the message in content,
                // not as a JSON-RPC error. That lets the model see and
                // potentially recover from the failure.
                let result = ToolsCallResult {
                    content: vec![ContentPart::Text {
                        text: format!("Ferridis dispatch failed: {e}"),
                    }],
                    is_error: Some(true),
                };
                Response::success(
                    id,
                    serde_json::to_value(result).expect("ToolsCallResult serializes"),
                )
            }
        }
    }

    async fn dispatch(
        &self,
        entry: &CatalogueEntry,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, ferridis_client::ClientError> {
        let capability = entry.capability();
        let intent = entry.intent().clone(); // allow:clone — dispatch takes the verb by value; the catalogue keeps its own
        // Normalize: MCP `arguments` may be omitted; pass through as `null`
        // turned into an empty object so adapters that expect an object
        // shape don't reject.
        let body = if arguments.is_null() {
            serde_json::json!({})
        } else {
            arguments
        };

        // Branch by intent kind: MCP 2024-11-05 has no progressive
        // streaming on `tools/call`, so stream-kind intents are
        // collected into the shared `CollectedStream` envelope (the
        // shape every stream-kind tool description advertises). When
        // MCP gains notifications-as-chunks, this is the call site that
        // grows progressive forwarding.
        match entry.kind() {
            IntentKind::Request => self.client.dispatch(capability, intent, body).await,
            IntentKind::Stream => {
                let stream = self
                    .client
                    .dispatch_streaming(capability, intent, body)
                    .await?;
                Ok(collect_stream(stream).await?.into_json())
            }
        }
    }
}

async fn write_response(
    writer: &mut BufWriter<tokio::io::Stdout>,
    response: &Response,
) -> std::io::Result<()> {
    let s = serde_json::to_string(response).expect("Response serializes");
    writer.write_all(s.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}
