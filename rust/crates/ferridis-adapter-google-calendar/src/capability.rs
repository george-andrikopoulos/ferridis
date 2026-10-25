//! [`GoogleCalendarCapability`] — the adapter's implementation of
//! [`ferridis_adapter_sdk::Capability`].

use async_trait::async_trait;
use ferridis_adapter_sdk::{Capability, DispatchError, SchemaSource};
use ferridis_core::{IntentVerb, Manifest};
use serde_json::Value;

use crate::client::GoogleCalendarClient;
use crate::types::{AccessToken, GoogleCalendarError, OAuthCredentials};

// ---------------------------------------------------------------------------
// Manifest + schema
// ---------------------------------------------------------------------------

const DEFAULT_MANIFEST_JSON: &str = r#"{
    "ferridis_version": "0.1",
    "id": "ferridis.google-calendar.v1",
    "name": "Ferridis Google Calendar",
    "category": "productivity",
    "summary": "List calendars, browse and manage events on Google Calendar.",
    "intents": [
        "list-calendars",
        "list-events",
        "get-event",
        "create-event",
        "update-event",
        "delete-event"
    ],
    "schema": {
        "type": "openapi-3",
        "url": "https://ferridis.io/schemas/google-calendar.v1.yaml"
    },
    "tiers": ["native"],
    "auth": { "type": "none" }
}"#;

/// The embedded OpenAPI 3 document, served at `GET /schema`.
///
/// JSON rather than YAML so `ferridis-protocol::ResponseSchemas` can read
/// it without a YAML dependency (decision PTC-2). Most intents pass the
/// Google Calendar API resource through, so the response schemas document
/// its stable fields. The Event shape is inlined into each intent rather
/// than shared through `components` + `$ref`, because a `$ref` cannot be
/// lifted out of the document into a tool description. Each schema is
/// pinned by a conformance assertion in `tests/google_calendar_adapter.rs`.
const EMBEDDED_SCHEMA: &str = r#"{
  "openapi": "3.0.3",
  "info": { "title": "Ferridis Google Calendar", "version": "0.1.0" },
  "paths": {
    "/intents/list-calendars": {
      "post": {
        "summary": "List all calendars the authenticated user has access to.",
        "requestBody": { "required": false, "content": { "application/json": { "schema": { "type": "object", "properties": {} } } } },
        "responses": { "200": { "description": "CalendarList resource.", "content": { "application/json": { "schema": {
          "type": "object", "required": ["items"],
          "properties": {
            "kind": { "type": "string", "const": "calendar#calendarList" },
            "items": {
              "type": "array",
              "items": {
                "type": "object", "required": ["id"],
                "properties": {
                  "id": { "type": "string", "description": "Pass as `calendar_id` to the other intents." },
                  "summary": { "type": "string" },
                  "primary": { "type": "boolean" },
                  "accessRole": { "type": "string" },
                  "timeZone": { "type": "string" }
                }
              }
            },
            "nextPageToken": { "type": "string" }
          }
        } } } } }
      }
    },
    "/intents/list-events": {
      "post": {
        "summary": "List events from a calendar.",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["calendar_id"],
          "properties": {
            "calendar_id": { "type": "string", "description": "Calendar identifier. Use \"primary\" for the user's primary calendar." },
            "max_results": { "type": "integer", "description": "Maximum number of events to return." },
            "time_min": { "type": "string", "format": "date-time", "description": "Lower bound (inclusive) for an event's end time (RFC3339)." },
            "time_max": { "type": "string", "format": "date-time", "description": "Upper bound (exclusive) for an event's start time (RFC3339)." }
          }
        } } } },
        "responses": { "200": { "description": "Events resource.", "content": { "application/json": { "schema": {
          "type": "object", "required": ["items"],
          "properties": {
            "kind": { "type": "string", "const": "calendar#events" },
            "items": {
              "type": "array",
              "items": {
                "type": "object", "required": ["id"],
                "properties": {
                  "id": { "type": "string" },
                  "status": { "type": "string", "enum": ["confirmed", "tentative", "cancelled"] },
                  "summary": { "type": "string" },
                  "description": { "type": "string" },
                  "htmlLink": { "type": "string" },
                  "start": { "type": "object", "properties": { "dateTime": { "type": "string" }, "date": { "type": "string" }, "timeZone": { "type": "string" } } },
                  "end": { "type": "object", "properties": { "dateTime": { "type": "string" }, "date": { "type": "string" }, "timeZone": { "type": "string" } } }
                }
              }
            },
            "nextPageToken": { "type": "string" }
          }
        } } } } }
      }
    },
    "/intents/get-event": {
      "post": {
        "summary": "Fetch a single event by ID.",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["calendar_id", "event_id"],
          "properties": { "calendar_id": { "type": "string" }, "event_id": { "type": "string" } }
        } } } },
        "responses": { "200": { "description": "Event resource.", "content": { "application/json": { "schema": {
          "type": "object", "required": ["id"],
          "properties": {
            "id": { "type": "string" },
            "status": { "type": "string", "enum": ["confirmed", "tentative", "cancelled"] },
            "summary": { "type": "string" },
            "description": { "type": "string" },
            "htmlLink": { "type": "string" },
            "start": { "type": "object", "properties": { "dateTime": { "type": "string" }, "date": { "type": "string" }, "timeZone": { "type": "string" } } },
            "end": { "type": "object", "properties": { "dateTime": { "type": "string" }, "date": { "type": "string" }, "timeZone": { "type": "string" } } },
            "attendees": { "type": "array", "items": { "type": "object", "properties": { "email": { "type": "string" }, "responseStatus": { "type": "string" } } } }
          }
        } } } } }
      }
    },
    "/intents/create-event": {
      "post": {
        "summary": "Create a new event in a calendar.",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["calendar_id", "summary", "start", "end"],
          "properties": {
            "calendar_id": { "type": "string" },
            "summary": { "type": "string" },
            "description": { "type": "string" },
            "start": { "type": "object", "description": "Event start time (Google Calendar EventDateTime object)." },
            "end": { "type": "object", "description": "Event end time (Google Calendar EventDateTime object)." }
          }
        } } } },
        "responses": { "200": { "description": "The created Event resource.", "content": { "application/json": { "schema": {
          "type": "object", "required": ["id"],
          "properties": {
            "id": { "type": "string" },
            "status": { "type": "string", "enum": ["confirmed", "tentative", "cancelled"] },
            "summary": { "type": "string" },
            "htmlLink": { "type": "string" },
            "start": { "type": "object", "properties": { "dateTime": { "type": "string" }, "date": { "type": "string" }, "timeZone": { "type": "string" } } },
            "end": { "type": "object", "properties": { "dateTime": { "type": "string" }, "date": { "type": "string" }, "timeZone": { "type": "string" } } }
          }
        } } } } }
      }
    },
    "/intents/update-event": {
      "post": {
        "summary": "Update an existing event.",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["calendar_id", "event_id"],
          "properties": {
            "calendar_id": { "type": "string" },
            "event_id": { "type": "string" },
            "summary": { "type": "string" },
            "description": { "type": "string" },
            "start": { "type": "object" },
            "end": { "type": "object" }
          }
        } } } },
        "responses": { "200": { "description": "The updated Event resource.", "content": { "application/json": { "schema": {
          "type": "object", "required": ["id"],
          "properties": {
            "id": { "type": "string" },
            "status": { "type": "string", "enum": ["confirmed", "tentative", "cancelled"] },
            "summary": { "type": "string" },
            "htmlLink": { "type": "string" },
            "start": { "type": "object", "properties": { "dateTime": { "type": "string" }, "date": { "type": "string" }, "timeZone": { "type": "string" } } },
            "end": { "type": "object", "properties": { "dateTime": { "type": "string" }, "date": { "type": "string" }, "timeZone": { "type": "string" } } }
          }
        } } } } }
      }
    },
    "/intents/delete-event": {
      "post": {
        "summary": "Delete an event from a calendar.",
        "requestBody": { "required": true, "content": { "application/json": { "schema": {
          "type": "object", "required": ["calendar_id", "event_id"],
          "properties": { "calendar_id": { "type": "string" }, "event_id": { "type": "string" } }
        } } } },
        "responses": { "200": { "description": "Deleted.", "content": { "application/json": { "schema": {
          "type": "object", "required": ["deleted"], "properties": { "deleted": { "type": "boolean", "const": true } }
        } } } } }
      }
    }
  }
}"#;

// ---------------------------------------------------------------------------
// GoogleCalendarCapability
// ---------------------------------------------------------------------------

/// Holds a [`GoogleCalendarClient`] configured with an operator-supplied
/// OAuth 2.0 access token. Optional refresh credentials enable automatic
/// token renewal on 401 responses.
pub struct GoogleCalendarCapability {
    manifest: Manifest,
    client: GoogleCalendarClient,
}

impl GoogleCalendarCapability {
    /// Build a capability from an access token.
    pub fn new(token: AccessToken) -> Result<Self, DispatchError> {
        let manifest = Manifest::parse(DEFAULT_MANIFEST_JSON).map_err(|e| {
            DispatchError::Internal(format!("default manifest failed to parse: {e}"))
        })?;
        Ok(Self {
            manifest,
            client: GoogleCalendarClient::new(token),
        })
    }

    /// Attach OAuth 2.0 refresh credentials for automatic token renewal.
    pub fn with_oauth_credentials(mut self, creds: OAuthCredentials) -> Self {
        self.client.oauth_creds = Some(creds);
        self
    }

    /// Override the Google Calendar API base URL (for tests or API proxies).
    pub fn with_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.client.api_base_url = url.into();
        self
    }

    /// Override the OAuth 2.0 token endpoint (for tests).
    pub fn with_token_url(mut self, url: impl Into<String>) -> Self {
        self.client.token_url = url.into();
        self
    }
}

// ---------------------------------------------------------------------------
// Capability impl
// ---------------------------------------------------------------------------

#[async_trait]
impl Capability for GoogleCalendarCapability {
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
            "list-calendars" => self.client.list_calendars().await.map_err(to_dispatch),

            "list-events" => {
                let cal_id = extract_str(&body, "calendar_id")?;
                let mut params: Vec<(&str, String)> = Vec::new();
                if let Some(n) = body.get("max_results").and_then(|v| v.as_u64()) {
                    params.push(("maxResults", n.to_string()));
                }
                if let Some(s) = body.get("time_min").and_then(|v| v.as_str()) {
                    params.push(("timeMin", s.to_owned()));
                }
                if let Some(s) = body.get("time_max").and_then(|v| v.as_str()) {
                    params.push(("timeMax", s.to_owned()));
                }
                let param_refs: Vec<(&str, &str)> =
                    params.iter().map(|(k, v)| (*k, v.as_str())).collect();
                self.client
                    .list_events(cal_id, &param_refs)
                    .await
                    .map_err(to_dispatch)
            }

            "get-event" => {
                let cal_id = extract_str(&body, "calendar_id")?;
                let event_id = extract_str(&body, "event_id")?;
                self.client
                    .get_event(cal_id, event_id)
                    .await
                    .map_err(to_dispatch)
            }

            "create-event" => {
                let cal_id = extract_str(&body, "calendar_id")?.to_owned();
                let event_body = strip_routing_fields(body);
                self.client
                    .create_event(&cal_id, &event_body)
                    .await
                    .map_err(to_dispatch)
            }

            "update-event" => {
                let cal_id = extract_str(&body, "calendar_id")?.to_owned();
                let event_id = extract_str(&body, "event_id")?.to_owned();
                let event_body = strip_routing_fields(body);
                self.client
                    .update_event(&cal_id, &event_id, &event_body)
                    .await
                    .map_err(to_dispatch)
            }

            "delete-event" => {
                let cal_id = extract_str(&body, "calendar_id")?.to_owned();
                let event_id = extract_str(&body, "event_id")?.to_owned();
                self.client
                    .delete_event(&cal_id, &event_id)
                    .await
                    .map_err(to_dispatch)?;
                Ok(serde_json::json!({ "deleted": true }))
            }

            _ => Err(DispatchError::UnsupportedIntent(intent.clone())), // allow:clone — SDK variant takes owned IntentVerb; small string
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn extract_str<'a>(body: &'a Value, key: &str) -> Result<&'a str, DispatchError> {
    body.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| DispatchError::InvalidRequest(format!("missing or non-string `{key}`")))
}

/// Remove `calendar_id` and `event_id` routing fields before forwarding
/// the remaining body as a Google Calendar event resource.
fn strip_routing_fields(mut body: Value) -> Value {
    if let Some(obj) = body.as_object_mut() {
        obj.remove("calendar_id");
        obj.remove("event_id");
    }
    body
}

fn to_dispatch(e: GoogleCalendarError) -> DispatchError {
    match e {
        GoogleCalendarError::ApiError {
            status: 403,
            message,
        } => DispatchError::Forbidden(message),
        GoogleCalendarError::ApiError {
            status: 404,
            message,
        } => DispatchError::NotFound(message),
        GoogleCalendarError::TokenExpired => {
            DispatchError::Forbidden("access token expired".into())
        }
        GoogleCalendarError::TokenRefreshFailed(msg) => {
            DispatchError::Forbidden(format!("token refresh failed: {msg}"))
        }
        other => DispatchError::Internal(other.to_string()),
    }
}
