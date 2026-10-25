//! Capability manifests — small, always-loaded summaries.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use url::Url;

use crate::caller::CallerPolicy;
use crate::error::FerridisError;
use crate::intent::IntentVerb;
use crate::tier::{Tier, Tiers};

/// A parsed and validated capability manifest.
///
/// Construction is restricted to [`Manifest::parse`]; downstream code can
/// rely on every field being present, well-typed, and within bounds.
#[derive(Debug, Clone)]
pub struct Manifest {
    ferridis_version: String,
    id: String,
    name: String,
    category: Category,
    summary: Summary,
    intents: BTreeSet<IntentVerb>,
    intent_metadata: BTreeMap<IntentVerb, IntentMetadata>,
    schema_url: Url,
    events_url: Option<Url>,
    event_channels: Vec<EventChannel>,
    endpoint_url: Option<Url>,
    tiers: Tiers,
    auth: AuthMethod,
}

/// The wire transport a capability uses for a named event channel.
///
/// Defaults to [`ChannelTransport::Sse`] when the `transport` field is
/// absent from the manifest, preserving backwards compatibility with
/// v0.2/v0.3 manifests that predate the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelTransport {
    /// Server-sent events (one-way, HTTP). The default.
    #[default]
    Sse,
    /// WebSocket (bidirectional). Use `Client::subscribe_ws` on the
    /// consumer side and the `WsHandler` trait on the publisher side.
    WebSocket,
}

/// Whether an intent returns a single response or a stream of
/// chunks. Defaults to [`IntentKind::Request`] when the manifest
/// uses the flat-string form for an intent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IntentKind {
    /// Single-shot request/response. Dispatch returns one
    /// [`serde_json::Value`].
    #[default]
    Request,
    /// Streamed response. The server emits an ordered sequence of
    /// chunks terminated by an `end` event. Use
    /// `Client::dispatch_streaming` (queued for v0.3 phase 2) to
    /// consume.
    Stream,
}

/// Per-intent metadata for an entry in [`Manifest::intent_metadata`].
///
/// Flat-string intents in the manifest's `intents` array get
/// `IntentMetadata::default()` (kind: Request, no chunk schema);
/// structured-form intents may carry an explicit kind and chunk
/// schema URL.
#[derive(Debug, Clone, Default)]
pub struct IntentMetadata {
    kind: IntentKind,
    chunk_schema_url: Option<Url>,
    callers: Option<CallerPolicy>,
}

impl IntentMetadata {
    /// Whether this intent returns a single response or a stream.
    #[must_use]
    pub fn kind(&self) -> IntentKind {
        self.kind
    }

    /// For streamed intents, the JSON Schema each chunk's `data`
    /// payload conforms to. `None` for request-kind intents.
    #[must_use]
    pub fn chunk_schema_url(&self) -> Option<&Url> {
        self.chunk_schema_url.as_ref()
    }

    /// Who the publisher says may call this intent as a model tool
    /// (protocol v0.7, optional; decision PTC-1). `None` when undeclared.
    #[must_use]
    pub fn declared_callers(&self) -> Option<CallerPolicy> {
        self.callers
    }
}

/// One named event channel a capability emits on.
///
/// Declared in the manifest's `event_channels` field. Clients use
/// the channel name to subscribe via the URL convention
/// `{endpoint_url}/events/{name}`. Access fields through the provided
/// getter methods — all fields are private so the validating constructor
/// owns the invariants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventChannel {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chunk_schema_url: Option<Url>,
    /// Wire transport for this channel. Defaults to [`ChannelTransport::Sse`].
    #[serde(default)]
    transport: ChannelTransport,
}

impl EventChannel {
    /// Parse and validate a channel definition. `name` is validated
    /// against the same rules as an intent verb to keep the
    /// `{base_url}/events/{name}` URL convention safe.
    pub fn parse(
        name: impl Into<String>,
        chunk_schema_url: Option<&str>,
        transport: ChannelTransport,
    ) -> Result<Self, FerridisError> {
        let name = name.into();
        // Reuse intent-verb validation: same character class, same
        // URL-safety properties.
        crate::intent::IntentVerb::parse(&name)?;
        let chunk_schema_url = match chunk_schema_url {
            Some(u) => Some(
                Url::parse(u)
                    .map_err(|e| FerridisError::Parse(format!("invalid chunk_schema_url: {e}")))?,
            ),
            None => None,
        };
        Ok(Self {
            name,
            chunk_schema_url,
            transport,
        })
    }

    /// Channel name. Same character class as [`IntentVerb`](crate::intent::IntentVerb):
    /// lowercase ASCII, digits, dashes; no leading/trailing/double dashes.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// JSON Schema URL describing each event's `data` payload, if declared.
    pub fn chunk_schema_url(&self) -> Option<&Url> {
        self.chunk_schema_url.as_ref()
    }

    /// The wire transport declared for this channel.
    pub fn transport(&self) -> ChannelTransport {
        self.transport
    }
}

impl Manifest {
    /// Parse a manifest from JSON.
    pub fn parse(json: &str) -> Result<Self, FerridisError> {
        let raw: RawManifest =
            serde_json::from_str(json).map_err(|e| FerridisError::Json(e.to_string()))?;
        raw.validate()
    }

    /// The Ferridis protocol version this manifest speaks.
    pub fn ferridis_version(&self) -> &str {
        &self.ferridis_version
    }

    /// The capability ID (e.g. `"google.calendar.v3"`).
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Human-readable name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The capability category (e.g. `"calendar"`).
    pub fn category(&self) -> &Category {
        &self.category
    }

    /// One-line summary, bounded to ~200 tokens of meaningful content.
    pub fn summary(&self) -> &Summary {
        &self.summary
    }

    /// The set of intent verbs this capability supports.
    pub fn intents(&self) -> &BTreeSet<IntentVerb> {
        &self.intents
    }

    /// Per-intent metadata: kind (Request/Stream) plus optional
    /// chunk schema URL for streamed intents. Manifests using the
    /// flat-string form for an intent yield default metadata
    /// (`IntentKind::Request`, no chunk schema).
    pub fn intent_metadata(&self, verb: &IntentVerb) -> Option<&IntentMetadata> {
        self.intent_metadata.get(verb)
    }

    /// Convenience: the kind of `verb`, or [`IntentKind::Request`]
    /// when the intent isn't declared (the caller would have already
    /// hit `IntentNotSupported` in that case; this just keeps lookup
    /// total).
    pub fn intent_kind(&self, verb: &IntentVerb) -> IntentKind {
        self.intent_metadata
            .get(verb)
            .map(|m| m.kind)
            .unwrap_or_default()
    }

    /// The publisher's caller declaration for `verb`, or `None` when the
    /// intent is undeclared or carries no `callers` field.
    #[must_use]
    pub fn declared_callers(&self, verb: &IntentVerb) -> Option<CallerPolicy> {
        self.intent_metadata
            .get(verb)
            .and_then(IntentMetadata::declared_callers)
    }

    /// Where to load the full schema (OpenAPI or AsyncAPI).
    pub fn schema_url(&self) -> &Url {
        &self.schema_url
    }

    /// Legacy single events URL. Kept for backwards compatibility
    /// with v0.2 manifests; prefer [`event_channels`](Self::event_channels)
    /// for new code.
    pub fn events_url(&self) -> Option<&Url> {
        self.events_url.as_ref()
    }

    /// Named event channels this capability emits on. Empty if the
    /// capability emits no events. Each channel's wire URL is
    /// `{endpoint_url}/events/{name}` by convention.
    pub fn event_channels(&self) -> &[EventChannel] {
        &self.event_channels
    }

    /// Whether the capability declares a channel with `name`.
    /// Cheap O(n) scan; v0.4 may add an indexed lookup if hosts
    /// subscribe to many channels per capability.
    pub fn has_event_channel(&self, name: &str) -> bool {
        self.event_channels.iter().any(|c| c.name == name)
    }

    /// The declared transport for a named event channel, or `None` if
    /// the channel is not declared. Use alongside [`has_event_channel`]
    /// to pick the right subscribe method on the client side.
    ///
    /// [`has_event_channel`]: Self::has_event_channel
    pub fn channel_transport(&self, name: &str) -> Option<ChannelTransport> {
        self.event_channels
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.transport)
    }

    /// Where to dispatch intent calls. The base URL the capability's
    /// `/intents/:verb` and `/events/:channel` routes live under.
    ///
    /// **Optional in v0.2** — when the manifest comes from a mesh
    /// registry the field is the authoritative endpoint location;
    /// when the manifest comes from the adapter itself the URL is
    /// implicit (it's where the manifest was served from) and this
    /// field may be absent. v0.3 will make this field required and
    /// remove the implicit fallback.
    pub fn endpoint_url(&self) -> Option<&Url> {
        self.endpoint_url.as_ref()
    }

    /// The execution tiers this capability supports (non-empty).
    pub fn tiers(&self) -> &Tiers {
        &self.tiers
    }

    /// The authentication method this capability requires.
    pub fn auth(&self) -> &AuthMethod {
        &self.auth
    }
}

/// A validated capability category.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Category(String);

impl Category {
    /// Parse and validate a category.
    pub fn parse(s: &str) -> Result<Self, FerridisError> {
        if s.is_empty() {
            return Err(FerridisError::MissingField("category"));
        }
        if !s.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
            return Err(FerridisError::Parse(format!(
                "category must be lowercase ASCII with dashes, got: {s}"
            )));
        }
        Ok(Self(s.to_string()))
    }

    /// The category as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Category {
    type Error = FerridisError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<Category> for String {
    fn from(c: Category) -> Self {
        c.0
    }
}

/// A capability summary, bounded to a hard byte limit (~200 tokens).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Summary(String);

const MAX_SUMMARY_BYTES: usize = 800;

impl Summary {
    /// Parse and validate a summary.
    pub fn parse(s: &str) -> Result<Self, FerridisError> {
        if s.is_empty() {
            return Err(FerridisError::MissingField("summary"));
        }
        if s.len() > MAX_SUMMARY_BYTES {
            return Err(FerridisError::OutOfBounds {
                field: "summary",
                detail: format!(
                    "summary is {} bytes, max is {} (~200 tokens)",
                    s.len(),
                    MAX_SUMMARY_BYTES
                ),
            });
        }
        Ok(Self(s.to_string()))
    }

    /// The summary text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Summary {
    type Error = FerridisError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::parse(&s)
    }
}

impl From<Summary> for String {
    fn from(s: Summary) -> Self {
        s.0
    }
}

/// The authentication method a capability requires.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthMethod {
    /// No authentication required.
    None,
    /// OAuth 2.0 / 2.1 with the listed scopes.
    Oauth2 {
        /// The OAuth scopes the connection requests.
        scopes: Vec<String>,
    },
    /// API key passed in a header.
    ApiKey {
        /// The header name to use (e.g. `"X-API-Key"`).
        header: String,
    },
}

#[derive(Debug, Deserialize)]
struct RawManifest {
    ferridis_version: Option<String>,
    id: Option<String>,
    name: Option<String>,
    category: Option<String>,
    summary: Option<String>,
    intents: Option<Vec<RawIntentEntry>>,
    schema: Option<RawSchemaRef>,
    events: Option<RawSchemaRef>,
    #[serde(default)]
    event_channels: Vec<RawEventChannel>,
    endpoint_url: Option<String>,
    tiers: Option<Vec<Tier>>,
    auth: Option<AuthMethod>,
}

#[derive(Debug, Deserialize)]
struct RawEventChannel {
    name: String,
    #[serde(default)]
    chunk_schema_url: Option<String>,
    #[serde(default)]
    transport: ChannelTransport,
}

/// Either a flat verb string (defaults to kind: Request) or a
/// structured object with explicit `kind` and optional
/// `chunk_schema_url`. Serde's `untagged` enum picks whichever
/// shape parses.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawIntentEntry {
    Flat(String),
    Structured {
        verb: String,
        #[serde(default)]
        kind: Option<IntentKind>,
        #[serde(default)]
        chunk_schema_url: Option<String>,
        #[serde(default)]
        callers: Option<CallerPolicy>,
    },
}

#[derive(Debug, Deserialize)]
struct RawSchemaRef {
    #[serde(rename = "type")]
    _ty: String,
    url: String,
}

impl RawManifest {
    fn validate(self) -> Result<Manifest, FerridisError> {
        let ferridis_version = self
            .ferridis_version
            .ok_or(FerridisError::MissingField("ferridis_version"))?;
        let id = self.id.ok_or(FerridisError::MissingField("id"))?;
        let name = self.name.ok_or(FerridisError::MissingField("name"))?;
        let category = Category::parse(
            &self
                .category
                .ok_or(FerridisError::MissingField("category"))?,
        )?;
        let summary = Summary::parse(&self.summary.ok_or(FerridisError::MissingField("summary"))?)?;

        let intents_raw = self.intents.ok_or(FerridisError::MissingField("intents"))?;
        if intents_raw.is_empty() {
            return Err(FerridisError::InvalidManifest(
                "intents must be non-empty".into(),
            ));
        }
        let mut intents = BTreeSet::new();
        let mut intent_metadata: BTreeMap<IntentVerb, IntentMetadata> = BTreeMap::new();
        for raw in &intents_raw {
            let (verb_str, kind, chunk_schema_url_raw, callers): (
                &str,
                IntentKind,
                Option<&str>,
                Option<CallerPolicy>,
            ) = match raw {
                RawIntentEntry::Flat(s) => (s.as_str(), IntentKind::default(), None, None),
                RawIntentEntry::Structured {
                    verb,
                    kind,
                    chunk_schema_url,
                    callers,
                } => (
                    verb.as_str(),
                    kind.unwrap_or_default(),
                    chunk_schema_url.as_deref(),
                    *callers,
                ),
            };
            let v = IntentVerb::parse(verb_str)?;
            if !intents.insert(v.clone()) {
                return Err(FerridisError::InvalidManifest(format!(
                    "duplicate intent verb: {verb_str}"
                )));
            }
            // Streamed intents must declare a chunk_schema_url; without
            // it, callers have no way to validate received chunks.
            if matches!(kind, IntentKind::Stream) && chunk_schema_url_raw.is_none() {
                return Err(FerridisError::InvalidManifest(format!(
                    "streamed intent `{verb_str}` must declare chunk_schema_url"
                )));
            }
            let chunk_schema_url = match chunk_schema_url_raw {
                Some(u) => Some(Url::parse(u).map_err(|e| {
                    FerridisError::Parse(format!(
                        "invalid chunk_schema_url for intent `{verb_str}`: {e}"
                    ))
                })?),
                None => None,
            };
            intent_metadata.insert(
                v,
                IntentMetadata {
                    kind,
                    chunk_schema_url,
                    callers,
                },
            );
        }

        let schema = self.schema.ok_or(FerridisError::MissingField("schema"))?;
        let schema_url = Url::parse(&schema.url)
            .map_err(|e| FerridisError::Parse(format!("invalid schema URL: {e}")))?;

        let events_url = match self.events {
            Some(e) => Some(
                Url::parse(&e.url)
                    .map_err(|err| FerridisError::Parse(format!("invalid events URL: {err}")))?,
            ),
            None => None,
        };

        let endpoint_url = match self.endpoint_url {
            Some(s) => Some(
                Url::parse(&s)
                    .map_err(|err| FerridisError::Parse(format!("invalid endpoint_url: {err}")))?,
            ),
            None => None,
        };

        let mut event_channels = Vec::with_capacity(self.event_channels.len());
        let mut seen_channel_names = BTreeSet::new();
        for raw in self.event_channels {
            let channel = EventChannel::parse(
                raw.name.clone(),
                raw.chunk_schema_url.as_deref(),
                raw.transport,
            )?; // clone: name is owned by raw which is consumed immediately after
            if !seen_channel_names.insert(channel.name.clone()) {
                return Err(FerridisError::InvalidManifest(format!(
                    "duplicate event channel name: {}",
                    raw.name
                )));
            }
            event_channels.push(channel);
        }

        let tiers = Tiers::from_vec(self.tiers.ok_or(FerridisError::MissingField("tiers"))?)?;
        let auth = self.auth.ok_or(FerridisError::MissingField("auth"))?;

        Ok(Manifest {
            ferridis_version,
            id,
            name,
            category,
            summary,
            intents,
            intent_metadata,
            schema_url,
            events_url,
            event_channels,
            endpoint_url,
            tiers,
            auth,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_MANIFEST: &str = r#"{
        "ferridis_version": "0.1",
        "id": "google.calendar.v3",
        "name": "Google Calendar",
        "category": "calendar",
        "summary": "Read events and find availability in Google Calendar.",
        "intents": ["read-events", "create-event", "find-time"],
        "schema": {
            "type": "openapi-3",
            "url": "https://calendar.googleapis.com/$discovery/rest?version=v3"
        },
        "tiers": ["native", "browser"],
        "auth": {
            "type": "oauth2",
            "scopes": ["https://www.googleapis.com/auth/calendar"]
        }
    }"#;

    #[test]
    fn parses_valid_manifest() {
        let m = Manifest::parse(VALID_MANIFEST).unwrap();
        assert_eq!(m.id(), "google.calendar.v3");
        assert_eq!(m.category().as_str(), "calendar");
        assert!(m.intents().iter().any(|i| i.as_str() == "read-events"));
        assert_eq!(m.tiers().preferred(), Tier::Native);
    }

    #[test]
    fn rejects_missing_fields() {
        let bad = r#"{"ferridis_version": "0.1"}"#;
        assert!(Manifest::parse(bad).is_err());
    }

    #[test]
    fn rejects_empty_intents() {
        let bad = VALID_MANIFEST.replace(
            r#""intents": ["read-events", "create-event", "find-time"]"#,
            r#""intents": []"#,
        );
        assert!(Manifest::parse(&bad).is_err());
    }

    #[test]
    fn rejects_duplicate_intents() {
        let bad = VALID_MANIFEST.replace(
            r#""intents": ["read-events", "create-event", "find-time"]"#,
            r#""intents": ["read-events", "read-events"]"#,
        );
        assert!(Manifest::parse(&bad).is_err());
    }

    #[test]
    fn rejects_invalid_intent_verb() {
        let bad = VALID_MANIFEST.replace("read-events", "Read_Events");
        assert!(Manifest::parse(&bad).is_err());
    }

    #[test]
    fn rejects_oversized_summary() {
        let huge = "x".repeat(900);
        let bad = VALID_MANIFEST.replace(
            "Read events and find availability in Google Calendar.",
            &huge,
        );
        assert!(Manifest::parse(&bad).is_err());
    }

    const MANIFEST_WITH_CHANNELS: &str = r#"{
        "ferridis_version": "0.1",
        "id": "test.events.v1",
        "name": "Events test capability",
        "category": "test",
        "summary": "Event-emitting test capability.",
        "intents": ["read-events"],
        "schema": { "type": "openapi-3", "url": "https://x/s.yaml" },
        "event_channels": [
            {"name": "state-changed"},
            {"name": "service-called", "chunk_schema_url": "https://x/svc.json"}
        ],
        "tiers": ["native"],
        "auth": { "type": "none" }
    }"#;

    #[test]
    fn parses_manifest_with_event_channels() {
        let m = Manifest::parse(MANIFEST_WITH_CHANNELS).unwrap();
        let channels = m.event_channels();
        assert_eq!(channels.len(), 2);
        assert_eq!(channels[0].name, "state-changed");
        assert!(channels[0].chunk_schema_url.is_none());
        assert_eq!(channels[1].name, "service-called");
        assert_eq!(
            channels[1].chunk_schema_url.as_ref().unwrap().as_str(),
            "https://x/svc.json"
        );
    }

    #[test]
    fn manifest_has_event_channel_lookup() {
        let m = Manifest::parse(MANIFEST_WITH_CHANNELS).unwrap();
        assert!(m.has_event_channel("state-changed"));
        assert!(m.has_event_channel("service-called"));
        assert!(!m.has_event_channel("never-declared"));
    }

    #[test]
    fn manifest_without_event_channels_field_parses_with_empty_vec() {
        // The VALID_MANIFEST at the top of this test module has no
        // event_channels field — verifies the default-empty path.
        let m = Manifest::parse(VALID_MANIFEST).unwrap();
        assert!(m.event_channels().is_empty());
    }

    #[test]
    fn rejects_event_channel_with_invalid_name() {
        let bad = MANIFEST_WITH_CHANNELS.replace("state-changed", "STATE_CHANGED");
        assert!(Manifest::parse(&bad).is_err());
    }

    #[test]
    fn rejects_duplicate_event_channel_names() {
        let bad = MANIFEST_WITH_CHANNELS.replace("service-called", "state-changed");
        let err = Manifest::parse(&bad).unwrap_err();
        match err {
            FerridisError::InvalidManifest(msg) => {
                assert!(msg.contains("duplicate"));
            }
            other => panic!("expected InvalidManifest for duplicate channel, got {other:?}"),
        }
    }

    const MANIFEST_WITH_STREAM_INTENT: &str = r#"{
        "ferridis_version": "0.1",
        "id": "test.stream.v1",
        "name": "Stream test capability",
        "category": "search",
        "summary": "Manifest mixing flat + structured intents.",
        "intents": [
            "read-event",
            {"verb": "search", "kind": "stream", "chunk_schema_url": "https://x/search.chunk.json"},
            {"verb": "list-events"}
        ],
        "schema": { "type": "openapi-3", "url": "https://x/s.yaml" },
        "tiers": ["native"],
        "auth": { "type": "none" }
    }"#;

    #[test]
    fn parses_mixed_flat_and_structured_intent_entries() {
        let m = Manifest::parse(MANIFEST_WITH_STREAM_INTENT).unwrap();
        assert_eq!(m.intents().len(), 3);

        let read_event = IntentVerb::parse("read-event").unwrap();
        let search = IntentVerb::parse("search").unwrap();
        let list_events = IntentVerb::parse("list-events").unwrap();

        // Flat-string intent → kind: Request, no chunk schema.
        assert_eq!(m.intent_kind(&read_event), IntentKind::Request);
        assert!(
            m.intent_metadata(&read_event)
                .unwrap()
                .chunk_schema_url
                .is_none()
        );

        // Structured stream intent → kind: Stream, chunk URL set.
        assert_eq!(m.intent_kind(&search), IntentKind::Stream);
        assert_eq!(
            m.intent_metadata(&search)
                .unwrap()
                .chunk_schema_url
                .as_ref()
                .unwrap()
                .as_str(),
            "https://x/search.chunk.json"
        );

        // Structured intent without explicit kind → defaults to Request.
        assert_eq!(m.intent_kind(&list_events), IntentKind::Request);
    }

    #[test]
    fn rejects_stream_intent_without_chunk_schema_url() {
        let bad = MANIFEST_WITH_STREAM_INTENT.replace(
            r#""kind": "stream", "chunk_schema_url": "https://x/search.chunk.json""#,
            r#""kind": "stream""#,
        );
        let err = Manifest::parse(&bad).unwrap_err();
        match err {
            FerridisError::InvalidManifest(msg) => {
                assert!(msg.contains("chunk_schema_url"));
            }
            other => panic!("expected InvalidManifest, got {other:?}"),
        }
    }

    /// PTC-1: a structured entry may declare `callers`; flat entries and
    /// structured entries without it stay undeclared (`None`).
    #[test]
    fn parses_the_optional_callers_declaration() {
        let json = MANIFEST_WITH_STREAM_INTENT.replace(
            r#"{"verb": "list-events"}"#,
            r#"{"verb": "list-events", "callers": "code"}"#,
        );
        let m = Manifest::parse(&json).expect("manifest with callers parses");
        let verb = |s: &str| IntentVerb::parse(s).expect("test verb is canonical");
        assert_eq!(
            m.declared_callers(&verb("list-events")),
            Some(CallerPolicy::Code)
        );
        assert_eq!(m.declared_callers(&verb("read-event")), None);
        assert_eq!(m.declared_callers(&verb("search")), None);
    }

    /// An unknown `callers` value fails the whole manifest rather than
    /// silently degrading to "undeclared": `"both"` is not a policy.
    #[test]
    fn rejects_an_unknown_callers_value() {
        let json = MANIFEST_WITH_STREAM_INTENT.replace(
            r#"{"verb": "list-events"}"#,
            r#"{"verb": "list-events", "callers": "both"}"#,
        );
        assert!(Manifest::parse(&json).is_err());
    }

    #[test]
    fn flat_intents_in_existing_manifests_still_parse() {
        // Existing VALID_MANIFEST uses only flat-string intents.
        // All of them should default to kind: Request with no chunk schema.
        let m = Manifest::parse(VALID_MANIFEST).unwrap();
        for v in m.intents() {
            assert_eq!(m.intent_kind(v), IntentKind::Request);
            assert!(
                m.intent_metadata(v)
                    .map(|md| md.chunk_schema_url.is_none())
                    .unwrap_or(true)
            );
        }
    }

    #[test]
    fn event_channel_parse_validates_name_and_url() {
        let ch = EventChannel::parse(
            "good-channel",
            Some("https://x/s.json"),
            ChannelTransport::Sse,
        )
        .unwrap(); // allow:unwrap
        assert_eq!(ch.name, "good-channel");
        assert!(EventChannel::parse("Bad_Name", None, ChannelTransport::Sse).is_err());
        assert!(EventChannel::parse("ok", Some("not-a-url"), ChannelTransport::Sse).is_err());
    }

    #[test]
    fn category_deserialize_runs_through_validator() {
        // Round-trip.
        let ok: Category = serde_json::from_str("\"calendar\"").unwrap();
        assert_eq!(ok.as_str(), "calendar");
        assert_eq!(serde_json::to_string(&ok).unwrap(), "\"calendar\"");

        // Pre-fix: serde(transparent) accepted these silently.
        for bad in [r#""""#, r#""Calendar""#, r#""cal_endar""#, r#""cal endar""#] {
            assert!(
                serde_json::from_str::<Category>(bad).is_err(),
                "JSON `{bad}` must not deserialize into Category"
            );
        }
    }

    #[test]
    fn summary_deserialize_runs_through_validator() {
        let ok: Summary = serde_json::from_str("\"a small valid summary\"").unwrap();
        assert_eq!(ok.as_str(), "a small valid summary");

        // Empty rejected at deserialize boundary.
        assert!(serde_json::from_str::<Summary>(r#""""#).is_err());

        // Oversized rejected at deserialize boundary.
        let huge = format!("\"{}\"", "x".repeat(900));
        assert!(serde_json::from_str::<Summary>(&huge).is_err());
    }
}
