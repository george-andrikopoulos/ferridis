//! Per-intent response schemas read from a capability's OpenAPI 3 document.
//!
//! [`fetch_schema`](crate::schema::fetch_schema) deliberately returns
//! opaque bytes. This module is the one parser built on top of those
//! bytes: it turns an OpenAPI 3 **JSON** document into a
//! [`ResponseSchemas`] witness. That is the per-intent JSON Schema of the
//! successful response, which tool projections append to a tool's
//! description so a model (or code calling the tool programmatically)
//! knows the shape of what comes back.
//!
//! Scope, deliberately narrow:
//!
//! - **JSON only.** OpenAPI 3 permits YAML or JSON. The workspace carries
//!   no YAML parser, and the reference adapters serve JSON. A YAML
//!   document fails with [`OpenApiError::NotJson`], and callers treat that
//!   as "no response schema known" (decision PTC-2 in `architecture.md`).
//! - **Ferridis route convention.** Only `POST /intents/{verb}` operations
//!   are read. Any other path is not an intent and is ignored.
//! - **Inline schemas only.** A response schema containing a `$ref` is
//!   meaningless once lifted out of its document. It is skipped with a
//!   WARN naming the intent, never forwarded half-resolved.

use std::collections::HashMap;

use ferridis_core::IntentVerb;
use serde_json::Value;

/// The route prefix every Ferridis intent is served under.
const INTENT_PATH_PREFIX: &str = "/intents/";

/// The media type whose schema is read from a response.
const JSON_MEDIA_TYPE: &str = "application/json";

/// The inline JSON Schema of one intent's successful response.
///
/// Constructed only by [`ResponseSchemas::from_openapi_json`], so a value
/// of this type is always an inline (`$ref`-free) schema lifted from a
/// `2xx` `application/json` response.
#[derive(Debug, Clone, PartialEq)]
pub struct ResponseSchema(Value);

impl ResponseSchema {
    /// Borrow the schema as JSON.
    #[must_use]
    pub fn as_json(&self) -> &Value {
        &self.0
    }
}

/// Response schemas for every intent the OpenAPI document describes.
///
/// An intent can be missing for three reasons: the document has no
/// operation for it, its success response is not JSON (stream-kind
/// intents answer with `text/event-stream`), or its schema uses `$ref`.
/// All three mean the same to a caller: "no response schema known".
#[derive(Debug, Clone, Default)]
pub struct ResponseSchemas {
    by_intent: HashMap<IntentVerb, ResponseSchema>,
}

/// Why an OpenAPI document could not be read at all.
///
/// Per-intent problems (an unparseable verb, a `$ref` schema) do not fail
/// the whole document. They skip that intent and log a WARN.
#[derive(Debug, thiserror::Error)]
pub enum OpenApiError {
    /// The document is not JSON. A YAML document lands here too.
    #[error("OpenAPI document is not JSON: {source}")]
    NotJson {
        /// The JSON parse failure.
        #[source]
        source: serde_json::Error,
    },
    /// The document parsed as JSON but does not declare `openapi: 3.x`.
    #[error("document is not OpenAPI 3 (`openapi` field: {found:?})")]
    NotOpenApi3 {
        /// The `openapi` field's value, if present and a string.
        found: Option<String>,
    },
}

impl ResponseSchemas {
    /// Parse an OpenAPI 3 JSON document into per-intent response schemas.
    ///
    /// # Errors
    ///
    /// [`OpenApiError::NotJson`] if `doc` is not JSON, and
    /// [`OpenApiError::NotOpenApi3`] if it lacks an `openapi: "3.…"` field.
    pub fn from_openapi_json(doc: &[u8]) -> Result<Self, OpenApiError> {
        let root: Value =
            serde_json::from_slice(doc).map_err(|source| OpenApiError::NotJson { source })?;
        let version = root.get("openapi").and_then(Value::as_str);
        if !version.is_some_and(|v| v.starts_with("3.")) {
            return Err(OpenApiError::NotOpenApi3 {
                found: version.map(str::to_owned),
            });
        }

        let mut by_intent = HashMap::new();
        let Some(paths) = root.get("paths").and_then(Value::as_object) else {
            return Ok(Self { by_intent });
        };
        for (path, item) in paths {
            let Some(verb_str) = path.strip_prefix(INTENT_PATH_PREFIX) else {
                continue;
            };
            let verb = match IntentVerb::parse(verb_str) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(path = %path, error = %e, "OpenAPI path is not a valid intent verb; skipped");
                    continue;
                }
            };
            let Some(schema) = success_json_schema(item) else {
                continue;
            };
            if contains_ref(schema) {
                tracing::warn!(
                    intent = %verb,
                    "response schema uses $ref, which cannot be lifted out of its document; skipped"
                );
                continue;
            }
            by_intent.insert(verb, ResponseSchema(schema.clone())); // allow:clone — the witness owns its schema; the parsed document is dropped at the end of this function
        }
        Ok(Self { by_intent })
    }

    /// The response schema for `intent`, if the document declares one.
    #[must_use]
    pub fn get(&self, intent: &IntentVerb) -> Option<&ResponseSchema> {
        self.by_intent.get(intent)
    }

    /// Number of intents with a known response schema.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_intent.len()
    }

    /// Whether no intent has a known response schema.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_intent.is_empty()
    }
}

/// The `application/json` schema of an operation's success response.
///
/// Prefers `"200"`. Otherwise takes the lowest `2xx` status that carries
/// a JSON schema.
fn success_json_schema(path_item: &Value) -> Option<&Value> {
    let responses = path_item.get("post")?.get("responses")?.as_object()?;
    let json_schema = |status: &str| {
        responses
            .get(status)?
            .get("content")?
            .get(JSON_MEDIA_TYPE)?
            .get("schema")
    };
    if let Some(schema) = json_schema("200") {
        return Some(schema);
    }
    let mut success: Vec<&str> = responses
        .keys()
        .map(String::as_str)
        .filter(|s| s.len() == 3 && s.starts_with('2'))
        .collect();
    success.sort_unstable();
    success.into_iter().find_map(json_schema)
}

/// Whether any object inside `schema` carries a `$ref` key.
fn contains_ref(schema: &Value) -> bool {
    match schema {
        Value::Object(map) => map.contains_key("$ref") || map.values().any(contains_ref),
        Value::Array(items) => items.iter().any(contains_ref),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn verb(s: &str) -> IntentVerb {
        IntentVerb::parse(s).expect("test verb is canonical")
    }

    fn doc(paths: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({ "openapi": "3.0.3", "info": {}, "paths": paths }))
            .expect("test document serializes")
    }

    #[test]
    fn extracts_the_json_schema_of_the_200_response() {
        let schema = json!({ "type": "object", "properties": { "ok": { "type": "boolean" } } });
        let d = doc(json!({
            "/intents/write-file": { "post": { "responses": {
                "200": { "description": "OK", "content": { "application/json": { "schema": schema } } }
            } } }
        }));
        let parsed = ResponseSchemas::from_openapi_json(&d).expect("valid document");
        assert_eq!(parsed.len(), 1);
        assert_eq!(
            parsed.get(&verb("write-file")).map(ResponseSchema::as_json),
            Some(&schema)
        );
    }

    #[test]
    fn falls_back_to_the_lowest_2xx_with_a_json_schema() {
        let d = doc(json!({
            "/intents/create-issue": { "post": { "responses": {
                "202": { "description": "x", "content": { "application/json": { "schema": { "type": "string" } } } },
                "201": { "description": "x", "content": { "application/json": { "schema": { "type": "object" } } } }
            } } }
        }));
        let parsed = ResponseSchemas::from_openapi_json(&d).expect("valid document");
        assert_eq!(
            parsed
                .get(&verb("create-issue"))
                .map(ResponseSchema::as_json),
            Some(&json!({ "type": "object" }))
        );
    }

    #[test]
    fn ignores_non_intent_paths_and_non_json_responses() {
        let d = doc(json!({
            "/healthz": { "post": { "responses": {
                "200": { "description": "x", "content": { "application/json": { "schema": { "type": "object" } } } }
            } } },
            "/intents/submit-prompt": { "post": { "responses": {
                "200": { "description": "SSE", "content": { "text/event-stream": {} } }
            } } },
            "/intents/read-file": { "post": { "responses": { "200": { "description": "OK" } } } }
        }));
        let parsed = ResponseSchemas::from_openapi_json(&d).expect("valid document");
        assert!(parsed.is_empty());
    }

    #[test]
    fn skips_a_schema_that_uses_ref() {
        let d = doc(json!({
            "/intents/get-page": { "post": { "responses": {
                "200": { "description": "x", "content": { "application/json": { "schema": {
                    "type": "object", "properties": { "parent": { "$ref": "#/components/schemas/Parent" } }
                } } } }
            } } }
        }));
        let parsed = ResponseSchemas::from_openapi_json(&d).expect("valid document");
        assert!(parsed.get(&verb("get-page")).is_none());
    }

    #[test]
    fn a_yaml_document_is_not_json() {
        let yaml = b"openapi: 3.0.3\ninfo:\n  title: x\npaths: {}\n";
        assert!(matches!(
            ResponseSchemas::from_openapi_json(yaml),
            Err(OpenApiError::NotJson { .. })
        ));
    }

    #[test]
    fn a_swagger_2_document_is_not_openapi_3() {
        let d = serde_json::to_vec(&json!({ "swagger": "2.0", "paths": {} }))
            .expect("test document serializes");
        assert!(matches!(
            ResponseSchemas::from_openapi_json(&d),
            Err(OpenApiError::NotOpenApi3 { found: None })
        ));
    }
}
