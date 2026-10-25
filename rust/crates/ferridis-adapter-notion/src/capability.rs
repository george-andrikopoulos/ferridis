//! [`NotionCapability`] — Ferridis [`Capability`] implementation for Notion.

use async_trait::async_trait;
use ferridis_adapter_sdk::{Capability, DispatchError, SchemaSource};
use ferridis_core::{IntentVerb, Manifest};
use serde_json::Value;

use crate::client::NotionClient;
use crate::types::{IntegrationToken, NotionError};

const DEFAULT_MANIFEST_JSON: &str = r#"{
    "ferridis_version": "0.1",
    "id": "ferridis.notion.v1",
    "name": "Ferridis Notion",
    "category": "knowledge",
    "summary": "Browse databases, query pages, and search your Notion workspace.",
    "intents": [
        "list-databases",
        "query-database",
        "get-page",
        "create-page",
        "update-page",
        "search"
    ],
    "schema": {
        "type": "openapi-3",
        "url": "https://ferridis.io/schemas/notion.v1.yaml"
    },
    "tiers": ["native"],
    "auth": { "type": "none" }
}"#;

/// The embedded OpenAPI 3 document, served at `GET /schema`.
///
/// JSON rather than YAML so `ferridis-protocol::ResponseSchemas` can read
/// it without a YAML dependency (decision PTC-2). Every intent passes the
/// Notion API object through, so the response schemas document Notion's
/// `list` and `page` envelopes. Each is pinned by a conformance assertion
/// in `tests/notion_adapter.rs`.
const EMBEDDED_SCHEMA: &str = r#"{
  "openapi": "3.0.3",
  "info": { "title": "Ferridis Notion", "version": "0.1.0" },
  "paths": {
    "/intents/list-databases": {
      "post": {
        "summary": "List all databases the integration has access to.",
        "requestBody": { "required": false, "content": { "application/json": { "schema": { "type": "object" } } } },
        "responses": { "200": { "description": "Notion search response filtered to databases.", "content": { "application/json": { "schema": {
          "type": "object", "required": ["object", "results"],
          "properties": {
            "object": { "type": "string", "const": "list" },
            "results": {
              "type": "array",
              "items": {
                "type": "object", "required": ["object", "id"],
                "properties": {
                  "object": { "type": "string", "const": "database" },
                  "id": { "type": "string" },
                  "title": { "type": "array", "description": "Notion rich-text array; the plain text is in each item's `plain_text`." },
                  "url": { "type": "string" }
                }
              }
            },
            "has_more": { "type": "boolean" },
            "next_cursor": { "type": ["string", "null"] }
          }
        } } } } }
      }
    },
    "/intents/query-database": {
      "post": {
        "summary": "Query a Notion database with optional filter and sorts.",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["database_id"],
          "properties": {
            "database_id": { "type": "string", "description": "ID of the database to query." },
            "filter": { "type": "object" },
            "sorts": { "type": "array", "items": { "type": "object" } },
            "start_cursor": { "type": "string" },
            "page_size": { "type": "integer" }
          }
        } } } },
        "responses": { "200": { "description": "Notion database query response (a list of pages).", "content": { "application/json": { "schema": {
          "type": "object", "required": ["object", "results"],
          "properties": {
            "object": { "type": "string", "const": "list" },
            "results": {
              "type": "array",
              "items": {
                "type": "object", "required": ["object", "id"],
                "properties": {
                  "object": { "type": "string", "const": "page" },
                  "id": { "type": "string" },
                  "properties": { "type": "object", "description": "Keyed by property name; each value is a typed Notion property object." },
                  "url": { "type": "string" }
                }
              }
            },
            "has_more": { "type": "boolean" },
            "next_cursor": { "type": ["string", "null"] }
          }
        } } } } }
      }
    },
    "/intents/get-page": {
      "post": {
        "summary": "Retrieve a Notion page by ID.",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["page_id"],
          "properties": { "page_id": { "type": "string", "description": "ID of the page to retrieve." } }
        } } } },
        "responses": { "200": { "description": "Notion page object.", "content": { "application/json": { "schema": {
          "type": "object", "required": ["object", "id"],
          "properties": {
            "object": { "type": "string", "const": "page" },
            "id": { "type": "string" },
            "properties": { "type": "object", "description": "Keyed by property name; each value is a typed Notion property object." },
            "archived": { "type": "boolean" },
            "url": { "type": "string" },
            "parent": { "type": "object" }
          }
        } } } } }
      }
    },
    "/intents/create-page": {
      "post": {
        "summary": "Create a new page in a Notion database or as a child page.",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["parent", "properties"],
          "properties": {
            "parent": { "type": "object", "description": "Parent database or page reference." },
            "properties": { "type": "object", "description": "Page properties matching the parent database schema." },
            "children": { "type": "array", "description": "Initial page content blocks." }
          }
        } } } },
        "responses": { "200": { "description": "Created Notion page object.", "content": { "application/json": { "schema": {
          "type": "object", "required": ["object", "id"],
          "properties": {
            "object": { "type": "string", "const": "page" },
            "id": { "type": "string" },
            "properties": { "type": "object" },
            "url": { "type": "string" }
          }
        } } } } }
      }
    },
    "/intents/update-page": {
      "post": {
        "summary": "Update properties or archive a Notion page.",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["page_id"],
          "properties": {
            "page_id": { "type": "string", "description": "ID of the page to update." },
            "properties": { "type": "object" },
            "archived": { "type": "boolean" }
          }
        } } } },
        "responses": { "200": { "description": "Updated Notion page object.", "content": { "application/json": { "schema": {
          "type": "object", "required": ["object", "id"],
          "properties": {
            "object": { "type": "string", "const": "page" },
            "id": { "type": "string" },
            "properties": { "type": "object" },
            "archived": { "type": "boolean" }
          }
        } } } } }
      }
    },
    "/intents/search": {
      "post": {
        "summary": "Search across the Notion workspace.",
        "requestBody": { "required": false, "content": { "application/json": { "schema": {
          "type": "object",
          "properties": {
            "query": { "type": "string" },
            "filter": { "type": "object" },
            "sort": { "type": "object" },
            "start_cursor": { "type": "string" },
            "page_size": { "type": "integer" }
          }
        } } } },
        "responses": { "200": { "description": "Notion search response (pages and databases).", "content": { "application/json": { "schema": {
          "type": "object", "required": ["object", "results"],
          "properties": {
            "object": { "type": "string", "const": "list" },
            "results": {
              "type": "array",
              "items": {
                "type": "object", "required": ["object", "id"],
                "properties": {
                  "object": { "type": "string", "enum": ["page", "database"] },
                  "id": { "type": "string" },
                  "url": { "type": "string" }
                }
              }
            },
            "has_more": { "type": "boolean" },
            "next_cursor": { "type": ["string", "null"] }
          }
        } } } } }
      }
    }
  }
}"#;

// ---------------------------------------------------------------------------
// NotionCapability
// ---------------------------------------------------------------------------

/// Holds a [`NotionClient`] configured with the operator-supplied integration token.
pub struct NotionCapability {
    manifest: Manifest,
    client: NotionClient,
}

impl NotionCapability {
    /// Build a capability from an integration token.
    pub fn new(token: IntegrationToken) -> Result<Self, DispatchError> {
        let manifest = Manifest::parse(DEFAULT_MANIFEST_JSON).map_err(|e| {
            DispatchError::Internal(format!("default manifest failed to parse: {e}"))
        })?;
        Ok(Self {
            manifest,
            client: NotionClient::new(token),
        })
    }

    /// Override the Notion API base URL (for tests).
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.client.api_base_url = url.into();
        self
    }
}

// ---------------------------------------------------------------------------
// Capability impl
// ---------------------------------------------------------------------------

#[async_trait]
impl Capability for NotionCapability {
    fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    fn schema(&self) -> SchemaSource {
        SchemaSource::Embedded {
            content_type: "application/json".into(),
            body: EMBEDDED_SCHEMA.into(),
        }
    }

    async fn dispatch(&self, intent: &IntentVerb, body: Value) -> Result<Value, DispatchError> {
        match intent.as_str() {
            "list-databases" => self.client.list_databases().await.map_err(to_dispatch),

            "query-database" => {
                let database_id = str_field(&body, "database_id")?.to_owned();
                let mut forward = body;
                if let Some(obj) = forward.as_object_mut() {
                    obj.remove("database_id");
                }
                self.client
                    .query_database(&database_id, forward)
                    .await
                    .map_err(to_dispatch)
            }

            "get-page" => {
                let page_id = str_field(&body, "page_id")?.to_owned();
                self.client.get_page(&page_id).await.map_err(to_dispatch)
            }

            "create-page" => self.client.create_page(body).await.map_err(to_dispatch),

            "update-page" => {
                let page_id = str_field(&body, "page_id")?.to_owned();
                let mut forward = body;
                if let Some(obj) = forward.as_object_mut() {
                    obj.remove("page_id");
                }
                self.client
                    .update_page(&page_id, forward)
                    .await
                    .map_err(to_dispatch)
            }

            "search" => self.client.search(body).await.map_err(to_dispatch),

            _ => Err(DispatchError::UnsupportedIntent(intent.clone())), // allow:clone — SDK variant takes owned IntentVerb; small string
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn str_field<'a>(body: &'a Value, key: &str) -> Result<&'a str, DispatchError> {
    body.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| DispatchError::InvalidRequest(format!("missing or non-string `{key}`")))
}

fn to_dispatch(e: NotionError) -> DispatchError {
    let msg = e.to_string();
    match e {
        NotionError::ApiError { status: 401, .. } | NotionError::ApiError { status: 403, .. } => {
            DispatchError::Forbidden(msg)
        }
        NotionError::ApiError { status: 404, .. } => DispatchError::NotFound(msg),
        _ => DispatchError::Internal(msg),
    }
}
