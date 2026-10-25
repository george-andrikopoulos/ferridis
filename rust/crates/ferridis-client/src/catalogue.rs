//! The tool catalogue: one projection of registered intents into named,
//! described tools, shared by every surface that publishes them.
//!
//! Each declared intent of each registered capability becomes one tool.
//! The tool name is `ferridis_<id_munged>_<intent_munged>`, with dots and
//! dashes replaced by underscores, because MCP and the Messages API both
//! want plain identifiers. `ferridis-mcp-server` projects this catalogue
//! into MCP `tools/list` entries, and `ferridis-ptc` projects it into
//! Messages-API custom tools. Because both read the same [`ToolName`],
//! an intent has one identity on every surface (decision PTC-3 in
//! `architecture.md`).
//!
//! Per-intent input schema precedence, highest first:
//!
//! 1. Schemas threaded through [`CatalogueSource::with_input_schemas`]
//!    (set when the upstream source, typically a federated MCP server,
//!    advertised typed schemas via `tools/list`). Republished verbatim so
//!    re-exposed tools keep their original parameter types.
//! 2. The hardcoded table in [`fallback_input_schema`], covering the
//!    filesystem reference adapter (`ferridis.fs.v1`).
//! 3. A permissive `{type: "object", additionalProperties: true}`.
//!
//! Every description ends with "Returns JSON." plus, when known, the
//! response's JSON Schema. For request-kind intents the schema comes from
//! the capability's OpenAPI document ([`crate::Client::load_response_schemas`]).
//! For stream-kind intents it is the fixed [`CollectedStream`] envelope,
//! because both surfaces collect a stream into that one value.

use std::collections::HashMap;
use std::sync::Arc;

use ferridis_core::{CapabilityRef, IntentKind, IntentVerb, Manifest};
use ferridis_protocol::{ResponseSchema, ResponseSchemas};
use futures_util::{Stream, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};

use crate::error::ClientError;
use crate::registry::CapabilityRecord;

/// A tool's name: unique per (manifest id, intent) and a plain identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ToolName(String);

impl ToolName {
    /// The name for `intent` on the capability whose manifest id is `manifest_id`.
    #[must_use]
    pub fn for_intent(manifest_id: &str, intent: &IntentVerb) -> Self {
        let id = manifest_id.replace(['.', '-'], "_");
        let verb = intent.as_str().replace('-', "_");
        Self(format!("ferridis_{id}_{verb}"))
    }

    /// Borrow the name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ToolName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::borrow::Borrow<str> for ToolName {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// A tool's human- and model-facing description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDescription(String);

impl ToolDescription {
    /// Borrow the description.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One registered capability, as the catalogue sees it.
#[derive(Debug, Clone)]
pub struct CatalogueSource {
    capability: CapabilityRef,
    manifest: Manifest,
    input_schemas: Option<Arc<HashMap<IntentVerb, Value>>>,
    response_schemas: Option<Arc<ResponseSchemas>>,
}

impl CatalogueSource {
    /// A source with no threaded input schemas and no response schemas.
    #[must_use]
    pub fn new(capability: CapabilityRef, manifest: Manifest) -> Self {
        Self {
            capability,
            manifest,
            input_schemas: None,
            response_schemas: None,
        }
    }

    /// Everything the registry knows about `record`: manifest, threaded
    /// input schemas and loaded response schemas.
    #[must_use]
    pub fn from_record(record: &CapabilityRecord) -> Self {
        Self {
            capability: record.capability().clone(), // allow:clone — the source outlives the registry lock the record is borrowed under
            manifest: record.manifest().clone(), // allow:clone — same: the catalogue is built after the lock is released
            input_schemas: record.input_schemas().map(Arc::clone),
            response_schemas: record.response_schemas().map(Arc::clone),
        }
    }

    /// Attach per-intent input schemas to republish verbatim.
    #[must_use]
    pub fn with_input_schemas(mut self, schemas: Option<Arc<HashMap<IntentVerb, Value>>>) -> Self {
        self.input_schemas = schemas;
        self
    }

    /// Attach per-intent response schemas.
    #[must_use]
    pub fn with_response_schemas(mut self, schemas: Option<Arc<ResponseSchemas>>) -> Self {
        self.response_schemas = schemas;
        self
    }

    /// The capability.
    #[must_use]
    pub fn capability(&self) -> &CapabilityRef {
        &self.capability
    }

    /// The capability's manifest.
    #[must_use]
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
}

/// One tool: a named, described projection of one (capability, intent).
#[derive(Debug, Clone)]
pub struct CatalogueEntry {
    name: ToolName,
    capability: CapabilityRef,
    intent: IntentVerb,
    kind: IntentKind,
    description: ToolDescription,
    input_schema: Value,
}

impl CatalogueEntry {
    /// The tool's name.
    #[must_use]
    pub fn name(&self) -> &ToolName {
        &self.name
    }

    /// The capability the tool dispatches to.
    #[must_use]
    pub fn capability(&self) -> &CapabilityRef {
        &self.capability
    }

    /// The intent the tool dispatches.
    #[must_use]
    pub fn intent(&self) -> &IntentVerb {
        &self.intent
    }

    /// Request or stream: which dispatch flavour the tool needs.
    #[must_use]
    pub fn kind(&self) -> IntentKind {
        self.kind
    }

    /// The tool's description, ending in its response format.
    #[must_use]
    pub fn description(&self) -> &ToolDescription {
        &self.description
    }

    /// The tool's input JSON Schema.
    #[must_use]
    pub fn input_schema(&self) -> &Value {
        &self.input_schema
    }
}

/// Every tool, in registration order, indexed by name.
#[derive(Debug, Default)]
pub struct Catalogue {
    entries: Vec<CatalogueEntry>,
    by_name: HashMap<ToolName, usize>,
}

impl Catalogue {
    /// Project `sources` into tools, one per declared intent.
    ///
    /// If two intents munge to the same name, both stay listed and
    /// [`Self::resolve`] returns the later one (the behaviour the MCP
    /// publisher has always had).
    #[must_use]
    pub fn from_sources(sources: &[CatalogueSource]) -> Self {
        let mut entries = Vec::new();
        let mut by_name = HashMap::new();
        for source in sources {
            for intent in source.manifest.intents() {
                let kind = source.manifest.intent_kind(intent);
                let name = ToolName::for_intent(source.manifest.id(), intent);
                let response = match kind {
                    IntentKind::Request => source
                        .response_schemas
                        .as_ref()
                        .and_then(|s| s.get(intent))
                        .map(ResponseSchema::as_json),
                    IntentKind::Stream => Some(collected_stream_schema()),
                };
                let description = describe(&source.manifest, intent, response);
                let input_schema = source
                    .input_schemas
                    .as_ref()
                    .and_then(|m| m.get(intent).cloned()) // allow:clone — each tool owns its schema; the threaded map stays shared by the registry
                    .unwrap_or_else(|| fallback_input_schema(source.manifest.id(), intent));
                by_name.insert(name.clone(), entries.len()); // allow:clone — the index key and the entry each own the name
                entries.push(CatalogueEntry {
                    name,
                    capability: source.capability.clone(), // allow:clone — each entry owns its target; sources are borrowed
                    intent: intent.clone(),                // allow:clone — same
                    kind,
                    description,
                    input_schema,
                });
            }
        }
        Self { entries, by_name }
    }

    /// The tool named `name`, if any.
    #[must_use]
    pub fn resolve(&self, name: &str) -> Option<&CatalogueEntry> {
        self.by_name.get(name).map(|&i| &self.entries[i])
    }

    /// Every tool, in registration order.
    #[must_use]
    pub fn entries(&self) -> &[CatalogueEntry] {
        &self.entries
    }
}

/// Build a tool description. It always says "Returns JSON." and carries
/// the response schema when one is known: programmatic callers parse the
/// result in code and need its shape up front.
fn describe(manifest: &Manifest, intent: &IntentVerb, response: Option<&Value>) -> ToolDescription {
    let mut text = format!(
        "Ferridis intent `{}` on capability `{}` ({}). {} Returns JSON.",
        intent.as_str(),
        manifest.id(),
        manifest.name(),
        manifest.summary().as_str()
    );
    if let Some(schema) = response {
        text.push_str(" Response schema: ");
        text.push_str(&schema.to_string());
    }
    ToolDescription(text)
}

/// What a stream-kind intent's chunks are collected into, so a surface
/// that answers with one value (an MCP 2024-11-05 `tools/call`, a
/// Messages-API `tool_result`) can carry a whole stream.
#[derive(Debug, Serialize)]
pub struct CollectedStream {
    result: String,
    chunks: Vec<Value>,
}

impl CollectedStream {
    /// The primary text: the last chunk's `result.result`, else the
    /// concatenated assistant text, else a chunk-count summary.
    #[must_use]
    pub fn result(&self) -> &str {
        &self.result
    }

    /// Every chunk, in arrival order.
    #[must_use]
    pub fn chunks(&self) -> &[Value] {
        &self.chunks
    }

    /// The JSON value both surfaces return.
    #[must_use]
    pub fn into_json(self) -> Value {
        json!({ "result": self.result, "chunks": self.chunks })
    }
}

/// The JSON Schema of [`CollectedStream::into_json`].
#[must_use]
pub fn collected_stream_schema() -> &'static Value {
    static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    SCHEMA.get_or_init(|| {
        json!({
            "type": "object",
            "required": ["result", "chunks"],
            "properties": {
                "result": { "type": "string", "description": "The stream's final result text." },
                "chunks": { "type": "array", "description": "Every streamed chunk, in order." }
            }
        })
    })
}

/// Drain a stream-kind dispatch into one [`CollectedStream`].
///
/// # Errors
///
/// The first error the stream yields; chunks before it are discarded.
pub async fn collect_stream<S>(stream: S) -> Result<CollectedStream, ClientError>
where
    S: Stream<Item = Result<Value, ClientError>>,
{
    let mut stream = std::pin::pin!(stream);
    let mut chunks = Vec::new();
    while let Some(item) = stream.next().await {
        chunks.push(item?);
    }
    let result = final_result_text(&chunks)
        .or_else(|| assistant_text(&chunks))
        .unwrap_or_else(|| {
            format!(
                "(stream produced {} chunks with no result text)",
                chunks.len()
            )
        });
    Ok(CollectedStream { result, chunks })
}

/// The last chunk shaped `{type: "result", result: "<text>"}`.
fn final_result_text(chunks: &[Value]) -> Option<String> {
    chunks.iter().rev().find_map(|c| {
        if c.get("type")? == "result" {
            c.get("result")?.as_str().map(str::to_owned)
        } else {
            None
        }
    })
}

/// Every `text` part of every `{type: "assistant"}` chunk, concatenated.
fn assistant_text(chunks: &[Value]) -> Option<String> {
    let text: String = chunks
        .iter()
        .filter(|c| c.get("type").and_then(Value::as_str) == Some("assistant"))
        .filter_map(|c| c.pointer("/message/content").and_then(Value::as_array))
        .flatten()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect();
    (!text.is_empty()).then_some(text)
}

/// Input schema for an intent with no threaded schema: the filesystem
/// reference adapter's table, else a permissive object.
///
/// Retiring this table in favour of the OpenAPI `requestBody` is a
/// separate TODO.md item: it would change client-side dispatch validation.
fn fallback_input_schema(manifest_id: &str, intent: &IntentVerb) -> Value {
    if manifest_id == "ferridis.fs.v1" {
        return match intent.as_str() {
            "read-file" => json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "Relative path under the adapter's root." }
                }
            }),
            "write-file" => json!({
                "type": "object",
                "required": ["path", "content"],
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                }
            }),
            "list-dir" => json!({
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "Empty string for the root." }
                }
            }),
            "search-files" => json!({
                "type": "object",
                "required": ["path", "name_contains"],
                "properties": {
                    "path": { "type": "string" },
                    "name_contains": { "type": "string" }
                }
            }),
            "move-file" => json!({
                "type": "object",
                "required": ["from", "to"],
                "properties": {
                    "from": { "type": "string" },
                    "to": { "type": "string" }
                }
            }),
            _ => permissive_object_schema(),
        };
    }
    permissive_object_schema()
}

fn permissive_object_schema() -> Value {
    json!({ "type": "object", "additionalProperties": true })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs_manifest(intents: &str) -> Manifest {
        Manifest::parse(&format!(
            r#"{{
                "ferridis_version": "0.1",
                "id": "ferridis.fs.v1",
                "name": "FS",
                "category": "files",
                "summary": "Filesystem.",
                "intents": {intents},
                "schema": {{ "type": "openapi-3", "url": "https://x/s.json" }},
                "tiers": ["native"],
                "auth": {{ "type": "none" }}
            }}"#
        ))
        .expect("test manifest parses")
    }

    fn fs_cap() -> CapabilityRef {
        CapabilityRef::parse("ferridis://public.ferridis.io/ferridis/fs@v1")
            .expect("test ref parses")
    }

    fn verb(s: &str) -> IntentVerb {
        IntentVerb::parse(s).expect("test verb is canonical")
    }

    fn response_schemas_for(intent: &str, schema: &Value) -> ResponseSchemas {
        let doc = json!({
            "openapi": "3.0.3",
            "paths": { format!("/intents/{intent}"): { "post": { "responses": { "200": {
                "description": "OK",
                "content": { "application/json": { "schema": schema } }
            } } } } }
        });
        ResponseSchemas::from_openapi_json(&serde_json::to_vec(&doc).expect("serializes"))
            .expect("test document parses")
    }

    #[test]
    fn tool_name_munges_dots_and_dashes() {
        assert_eq!(
            ToolName::for_intent("ferridis.fs.v1", &verb("read-file")).as_str(),
            "ferridis_ferridis_fs_v1_read_file"
        );
    }

    #[test]
    fn catalogue_indexes_each_intent() {
        let cat = Catalogue::from_sources(&[CatalogueSource::new(
            fs_cap(),
            fs_manifest(r#"["read-file", "write-file"]"#),
        )]);
        assert_eq!(cat.entries().len(), 2);
        let entry = cat
            .resolve("ferridis_ferridis_fs_v1_read_file")
            .expect("read-file resolves");
        assert_eq!(entry.capability(), &fs_cap());
        assert_eq!(entry.intent().as_str(), "read-file");
    }

    /// The PTC step-2 pin: a known response schema is carried in the
    /// description verbatim, after "Returns JSON.".
    #[test]
    fn description_carries_the_response_schema_when_known() {
        let schema = json!({ "type": "object", "required": ["content"], "properties": { "content": { "type": "string" } } });
        let source = CatalogueSource::new(fs_cap(), fs_manifest(r#"["read-file"]"#))
            .with_response_schemas(Some(Arc::new(response_schemas_for("read-file", &schema))));
        let cat = Catalogue::from_sources(&[source]);
        let description = cat.entries()[0].description().as_str();
        assert!(
            description.ends_with(&format!("Returns JSON. Response schema: {schema}")),
            "{description}"
        );
    }

    #[test]
    fn description_says_returns_json_even_without_a_schema() {
        let cat = Catalogue::from_sources(&[CatalogueSource::new(
            fs_cap(),
            fs_manifest(r#"["read-file"]"#),
        )]);
        let description = cat.entries()[0].description().as_str();
        assert!(description.ends_with("Returns JSON."), "{description}");
    }

    /// A stream-kind intent describes the collected envelope, whatever the
    /// OpenAPI document says, because that envelope is what the caller gets.
    #[test]
    fn stream_kind_description_carries_the_collected_envelope_schema() {
        let manifest = fs_manifest(
            r#"[{ "verb": "tail-log", "kind": "stream", "chunk_schema_url": "https://x/c.json" }]"#,
        );
        let cat = Catalogue::from_sources(&[CatalogueSource::new(fs_cap(), manifest)]);
        let entry = &cat.entries()[0];
        assert_eq!(entry.kind(), IntentKind::Stream);
        assert!(
            entry
                .description()
                .as_str()
                .ends_with(&collected_stream_schema().to_string()),
            "{}",
            entry.description().as_str()
        );
    }

    /// The envelope schema in the description is the shape `collect_stream`
    /// really produces. If either drifts, this fails.
    #[tokio::test]
    async fn collected_stream_matches_its_advertised_schema() {
        let chunks = vec![
            Ok(
                json!({ "type": "assistant", "message": { "content": [{ "type": "text", "text": "PO" }] } }),
            ),
            Ok(json!({ "type": "result", "result": "PONG" })),
        ];
        let collected = collect_stream(futures_util::stream::iter(chunks))
            .await
            .expect("stream collects");
        assert_eq!(collected.result(), "PONG");
        let value = collected.into_json();
        let validator =
            jsonschema::validator_for(collected_stream_schema()).expect("envelope schema compiles");
        assert!(validator.is_valid(&value), "{value}");
    }

    #[tokio::test]
    async fn collect_stream_falls_back_to_assistant_text_then_a_count() {
        let assistant = vec![Ok(json!({ "type": "assistant", "message": { "content": [
            { "type": "text", "text": "Hel" }, { "type": "text", "text": "lo" }
        ] } }))];
        let collected = collect_stream(futures_util::stream::iter(assistant))
            .await
            .expect("stream collects");
        assert_eq!(collected.result(), "Hello");

        let bare = vec![Ok(json!({ "n": 1 })), Ok(json!({ "n": 2 }))];
        let collected = collect_stream(futures_util::stream::iter(bare))
            .await
            .expect("stream collects");
        assert_eq!(
            collected.result(),
            "(stream produced 2 chunks with no result text)"
        );
    }

    /// MCP-backed adapter: a threaded upstream `inputSchema` is
    /// republished verbatim. Regression guard for the `limit=5`
    /// integer-stringification bug.
    #[test]
    fn catalogue_prefers_threaded_input_schemas_over_fallback() {
        let manifest = Manifest::parse(
            r#"{
            "ferridis_version": "0.1",
            "id": "mcp.linux-health-mcp.v1",
            "name": "MCP — linux-health-mcp",
            "category": "external-mcp",
            "summary": "Synthetic.",
            "intents": ["journal-query"],
            "schema": { "type": "mcp-tools-list", "url": "mcp+tools-list:mcp.linux-health-mcp.v1" },
            "tiers": ["native"],
            "auth": { "type": "none" }
        }"#,
        )
        .expect("test manifest parses");
        let cap = CapabilityRef::parse("ferridis://wallet/mcp/linux-health-mcp@v1")
            .expect("test ref parses");
        let upstream = json!({
            "type": "object",
            "properties": { "limit": { "type": "integer", "minimum": 1, "maximum": 1000 } },
            "additionalProperties": false
        });
        let mut schemas = HashMap::new();
        schemas.insert(verb("journal-query"), upstream.clone()); // allow:clone — the test compares against its own copy
        let cat = Catalogue::from_sources(&[
            CatalogueSource::new(cap, manifest).with_input_schemas(Some(Arc::new(schemas)))
        ]);
        let entry = cat
            .resolve(
                ToolName::for_intent("mcp.linux-health-mcp.v1", &verb("journal-query")).as_str(),
            )
            .expect("journal-query tool must be present");
        assert_eq!(entry.input_schema(), &upstream);
    }

    #[test]
    fn catalogue_falls_back_to_hardcoded_fs_schema() {
        let cat = Catalogue::from_sources(&[CatalogueSource::new(
            fs_cap(),
            fs_manifest(r#"["read-file"]"#),
        )]);
        let required = cat.entries()[0]
            .input_schema()
            .get("required")
            .and_then(Value::as_array)
            .expect("read-file schema must declare required fields");
        assert!(required.iter().any(|v| v == "path"));
    }

    #[test]
    fn catalogue_falls_back_to_permissive_when_no_schema_is_known() {
        let manifest = Manifest::parse(
            r#"{
            "ferridis_version": "0.1",
            "id": "mcp.unknown-server.v1",
            "name": "MCP — unknown",
            "category": "external-mcp",
            "summary": "Synthetic.",
            "intents": ["do-something"],
            "schema": { "type": "mcp-tools-list", "url": "mcp+tools-list:mcp.unknown-server.v1" },
            "tiers": ["native"],
            "auth": { "type": "none" }
        }"#,
        )
        .expect("test manifest parses");
        let cap = CapabilityRef::parse("ferridis://wallet/mcp/unknown-server@v1")
            .expect("test ref parses");
        let cat = Catalogue::from_sources(&[CatalogueSource::new(cap, manifest)]);
        assert_eq!(
            cat.entries()[0].input_schema().get("additionalProperties"),
            Some(&Value::Bool(true)),
        );
    }
}
