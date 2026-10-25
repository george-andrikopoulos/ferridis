//! Response conformance: does an adapter return what its OpenAPI says?
//!
//! Tool projections (`ferridis-client::catalogue`) append each intent's
//! declared response schema to the tool description that a model reads.
//! If the declared schema drifts from what the adapter really returns,
//! that description misinforms the model, and code that parses the
//! result programmatically breaks on the first call. [`check_response`]
//! is the check that keeps the two in step. Each reference adapter's e2e
//! suite runs it against the document the adapter actually serves at
//! `GET /schema`.
//!
//! Unlike [`validate_body`](crate::validate_body), this check is **strict**.
//! A missing declaration or a schema that does not compile is a failure,
//! not a pass: this is a test-time contract, not a runtime gate.

use ferridis_core::IntentVerb;
use ferridis_protocol::{OpenApiError, ResponseSchemas};
use serde_json::Value;

use crate::validation::ValidationError;

/// The outcome of checking one response against its declared schema.
#[must_use]
#[derive(Debug)]
pub enum ResponseConformance {
    /// The response validates against the declared schema.
    Conforms,
    /// The document parsed, but declares no JSON response schema for this intent.
    NoSchemaDeclared,
    /// The declared schema is not a valid JSON Schema.
    SchemaDoesNotCompile {
        /// The compiler's message.
        detail: String,
    },
    /// The response violates the declared schema.
    Violates {
        /// One entry per violated constraint.
        errors: Vec<ValidationError>,
    },
}

impl ResponseConformance {
    /// Whether the response conforms.
    #[must_use]
    pub fn is_conforming(&self) -> bool {
        matches!(self, Self::Conforms)
    }
}

/// Check `response` (an intent's actual output) against the response
/// schema that `schema_document` (an OpenAPI 3 JSON document) declares
/// for `intent`.
///
/// # Errors
///
/// Returns the [`OpenApiError`] when the document itself cannot be read.
pub fn check_response(
    schema_document: &[u8],
    intent: &IntentVerb,
    response: &Value,
) -> Result<ResponseConformance, OpenApiError> {
    let schemas = ResponseSchemas::from_openapi_json(schema_document)?;
    let Some(schema) = schemas.get(intent) else {
        return Ok(ResponseConformance::NoSchemaDeclared);
    };
    let compiled = match jsonschema::validator_for(schema.as_json()) {
        Ok(v) => v,
        Err(e) => {
            return Ok(ResponseConformance::SchemaDoesNotCompile {
                detail: e.to_string(),
            });
        }
    };
    let errors: Vec<ValidationError> = compiled
        .iter_errors(response)
        .map(|e| ValidationError {
            instance_path: e.instance_path.to_string(),
            detail: e.to_string(),
        })
        .collect();
    Ok(if errors.is_empty() {
        ResponseConformance::Conforms
    } else {
        ResponseConformance::Violates { errors }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "openapi": "3.0.3",
            "info": {},
            "paths": {
                "/intents/write-file": { "post": { "responses": { "200": {
                    "description": "OK",
                    "content": { "application/json": { "schema": {
                        "type": "object", "required": ["ok"],
                        "properties": { "ok": { "type": "boolean" } }
                    } } }
                } } } },
                "/intents/move-file": { "post": { "responses": { "200": { "description": "OK" } } } }
            }
        }))
        .expect("test document serializes")
    }

    fn verb(s: &str) -> IntentVerb {
        IntentVerb::parse(s).expect("test verb is canonical")
    }

    #[test]
    fn conforming_response_conforms() {
        let outcome = check_response(&doc(), &verb("write-file"), &json!({ "ok": true }))
            .expect("document parses");
        assert!(outcome.is_conforming(), "{outcome:?}");
    }

    #[test]
    fn wrong_shape_is_a_violation() {
        let outcome =
            check_response(&doc(), &verb("write-file"), &json!([true])).expect("document parses");
        assert!(
            matches!(outcome, ResponseConformance::Violates { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn undeclared_response_schema_is_not_a_pass() {
        let outcome = check_response(&doc(), &verb("move-file"), &json!({ "ok": true }))
            .expect("document parses");
        assert!(matches!(outcome, ResponseConformance::NoSchemaDeclared));
    }
}
