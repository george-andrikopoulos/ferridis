//! HTTP server that hosts a single [`Capability`].
//!
//! The route table is derived from the capability's declared intents,
//! so wiring an intent the manifest does not list is impossible by
//! construction:
//!
//! - `GET /manifest.json` — serves the capability's manifest JSON.
//! - `GET /schema` — serves the schema (or redirects to its URL).
//! - `POST /intents/<verb>` — invokes [`Capability::dispatch`] for each
//!   intent verb declared in the manifest.
//!
//! Intent verbs that are not in the manifest receive `404` automatically
//! because no route is mounted for them.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use ferridis_core::{IntentKind, IntentVerb};
use ferridis_protocol::BackpressureSignal;
use futures_util::StreamExt;
use std::convert::Infallible;
use std::time::Duration;
use tokio::net::TcpListener;

use crate::capability::{Capability, SchemaSource};

/// HTTP server hosting one [`Capability`].
///
/// Built from a value, not configured by mutation — see the docs for
/// rationale on the type-driven discipline.
pub struct AdapterServer {
    router: Router,
}

impl AdapterServer {
    /// Build a new server hosting `capability`.
    pub fn new<C: Capability>(capability: C) -> Self {
        let arc: Arc<dyn Capability> = Arc::new(capability);

        // A single dynamic `:verb` route handles all intents. The
        // handler then checks the verb against the manifest's declared
        // intent set, so wiring an undeclared verb is rejected with 404
        // without the SDK having to mount per-intent routes.
        let router = Router::new()
            .route("/manifest.json", get(serve_manifest))
            .route("/schema", get(serve_schema))
            .route("/intents/:verb", post(dispatch_intent))
            .with_state(arc);
        Self { router }
    }

    /// The composed [`axum::Router`]. Exposed for callers that want to
    /// merge the adapter into a larger application.
    pub fn into_router(self) -> Router {
        self.router
    }

    /// Bind to `addr` and serve forever.
    pub async fn serve(self, addr: SocketAddr) -> std::io::Result<()> {
        let listener = TcpListener::bind(addr).await?;
        axum::serve(listener, self.router).await
    }
}

async fn serve_manifest(State(cap): State<Arc<dyn Capability>>) -> Response {
    let m = cap.manifest();
    // Re-serialize from the manifest's public accessors. The core
    // crate's `Manifest` does not currently expose a serializer of its
    // own (it's parse-only by design) so we build the JSON manually
    // from accessors here.
    //
    // Each intent is emitted in the **structured form** when its
    // metadata is non-default (kind != Request or a chunk schema is
    // declared); otherwise as a flat string. This preserves the
    // manifest's intent-kind declarations across the SDK round-trip
    // so a stream-kind intent stays stream-kind on the client side.
    let intents: Vec<serde_json::Value> = m
        .intents()
        .iter()
        .map(|v| {
            let meta = m.intent_metadata(v);
            let is_default = match meta {
                None => true,
                Some(md) => {
                    matches!(md.kind(), IntentKind::Request)
                        && md.chunk_schema_url().is_none()
                        && md.declared_callers().is_none()
                }
            };
            if is_default {
                serde_json::Value::String(v.as_str().to_string())
            } else {
                let md = meta.expect("non-default implies present");
                let kind_str = match md.kind() {
                    IntentKind::Request => "request",
                    IntentKind::Stream => "stream",
                };
                let mut obj = serde_json::Map::new();
                obj.insert(
                    "verb".to_string(),
                    serde_json::Value::String(v.as_str().to_string()),
                );
                obj.insert(
                    "kind".to_string(),
                    serde_json::Value::String(kind_str.to_string()),
                );
                if let Some(url) = md.chunk_schema_url() {
                    obj.insert(
                        "chunk_schema_url".to_string(),
                        serde_json::Value::String(url.as_str().to_string()),
                    );
                }
                if let Some(callers) = md.declared_callers() {
                    obj.insert(
                        "callers".to_string(),
                        serde_json::Value::String(callers.to_string()),
                    );
                }
                serde_json::Value::Object(obj)
            }
        })
        .collect();
    let tiers: Vec<String> = m
        .tiers()
        .iter()
        .map(|t| match t {
            ferridis_core::Tier::Native => "native".into(),
            ferridis_core::Tier::Browser => "browser".into(),
            ferridis_core::Tier::Vision => "vision".into(),
        })
        .collect();
    let auth = match m.auth() {
        ferridis_core::AuthMethod::None => serde_json::json!({"type": "none"}),
        ferridis_core::AuthMethod::Oauth2 { scopes } => {
            serde_json::json!({"type": "oauth2", "scopes": scopes})
        }
        ferridis_core::AuthMethod::ApiKey { header } => {
            serde_json::json!({"type": "api_key", "header": header})
        }
    };
    let body = serde_json::json!({
        "ferridis_version": m.ferridis_version(),
        "id": m.id(),
        "name": m.name(),
        "category": m.category().as_str(),
        "summary": m.summary().as_str(),
        "intents": intents,
        "schema": {"type": "openapi-3", "url": m.schema_url().as_str()},
        "tiers": tiers,
        "auth": auth,
    });
    Json(body).into_response()
}

async fn serve_schema(State(cap): State<Arc<dyn Capability>>) -> Response {
    match cap.schema() {
        SchemaSource::Embedded { content_type, body } => {
            (StatusCode::OK, [(header::CONTENT_TYPE, content_type)], body).into_response()
        }
        SchemaSource::Redirect { url } => Redirect::temporary(url.as_str()).into_response(),
    }
}

async fn dispatch_intent(
    State(cap): State<Arc<dyn Capability>>,
    Path(verb_str): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let verb = match IntentVerb::parse(&verb_str) {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid intent verb"})),
            )
                .into_response();
        }
    };

    if !cap.manifest().intents().contains(&verb) {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "intent not supported"})),
        )
            .into_response();
    }

    // v0.4: validate the request body against the adapter's declared
    // schema before dispatch. Adapters that don't implement body_schema
    // return None and skip validation transparently.
    if let Some(schema) = cap.body_schema(&verb) {
        match crate::validation::validate_body(&schema, &body) {
            crate::validation::ValidationOutcome::Valid => {}
            crate::validation::ValidationOutcome::Invalid { errors } => {
                let details: Vec<serde_json::Value> = errors
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "instance_path": e.instance_path,
                            "detail": e.detail,
                        })
                    })
                    .collect();
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(serde_json::json!({
                        "error": "request body does not conform to the declared schema",
                        "validation_errors": details,
                    })),
                )
                    .into_response();
            }
        }
    }

    // Route by manifest-declared intent kind: Stream goes through
    // `dispatch_stream` + SSE; Request goes through `dispatch` + JSON.
    // The SDK never asks an adapter to emit the wrong shape.
    match cap.manifest().intent_kind(&verb) {
        IntentKind::Stream => dispatch_stream_intent(cap, verb, body).await,
        IntentKind::Request => match cap.dispatch(&verb, body).await {
            Ok(v) => (StatusCode::OK, Json(v)).into_response(),
            Err(e) => (
                StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response(),
        },
    }
}

/// Bridge an adapter's [`Capability::dispatch_stream`] into an SSE
/// response. Each `Ok(value)` from the adapter's stream becomes a
/// `chunk` event; an `Err` produces one `error` event then closes;
/// natural stream-exhaustion emits a final `end` event so the
/// client can distinguish "done" from "connection dropped."
async fn dispatch_stream_intent(
    cap: Arc<dyn Capability>,
    verb: IntentVerb,
    body: serde_json::Value,
) -> Response {
    // The dispatch_stream call itself can fail (e.g., body parse,
    // permission check) before any chunks have streamed. Surface
    // those failures as a one-shot SSE response carrying a single
    // `error` event, rather than degrading to JSON — the client is
    // already in SSE-decoding mode.
    let stream_result = cap.dispatch_stream_flow(&verb, body).await;
    let inner_stream = match stream_result {
        Ok(s) => s,
        Err(e) => {
            let err_event = SseEvent::default().event("error").data(
                serde_json::to_string(&serde_json::json!({
                    "status": e.status_code(),
                    "message": e.to_string(),
                }))
                .unwrap_or_else(|_| String::from("\"adapter error\"")),
            );
            let end_event = SseEvent::default().event("end").data("{}");
            let one_shot = futures_util::stream::iter(vec![
                Ok::<SseEvent, Infallible>(err_event),
                Ok(end_event),
            ]);
            return Sse::new(one_shot)
                .keep_alive(KeepAlive::new().interval(Duration::from_secs(60)))
                .into_response();
        }
    };

    // Map the adapter's flow-aware stream into an SSE stream-of-events.
    //
    // Backpressure wiring (v0.7): the signal travels as a separate
    // `event: backpressure` emitted only when it *changes* from the
    // previous state (`Continue` is the implicit start, never sent —
    // pre-v0.7 clients ignore the event per SSE convention). `Halt`
    // terminates the stream: the halt event is the last thing sent, no
    // trailing `end`, so consumers see a deliberate stop rather than a
    // clean finish.
    let events = async_stream::stream! {
        let mut inner = inner_stream;
        let mut current_signal = BackpressureSignal::Continue;
        while let Some(item) = inner.next().await {
            match item {
                Ok(chunk) => {
                    let signal = chunk.signal();
                    if signal != current_signal {
                        current_signal = signal;
                        yield Ok::<SseEvent, Infallible>(
                            SseEvent::default().event("backpressure").data(
                                serde_json::to_string(&serde_json::json!({"signal": signal}))
                                    .unwrap_or_else(|_| String::from("{\"signal\":\"halt\"}")),
                            ),
                        );
                    }
                    if signal == BackpressureSignal::Halt {
                        // The adapter declared itself overwhelmed; the
                        // chunk that carried the halt is NOT delivered —
                        // halt means "stop", not "one more".
                        return;
                    }
                    yield Ok(SseEvent::default().event("chunk").data(
                        serde_json::to_string(chunk.data())
                            .unwrap_or_else(|_| String::from("null")),
                    ));
                }
                Err(e) => {
                    yield Ok(SseEvent::default().event("error").data(
                        serde_json::to_string(&serde_json::json!({
                            "status": e.status_code(),
                            "message": e.to_string(),
                        }))
                        .unwrap_or_else(|_| String::from("\"adapter error\"")),
                    ));
                    return;
                }
            }
        }
        // Natural exhaustion: a final `event: end` so the consumer-side
        // parser knows it's a clean termination, not a dropped
        // connection.
        yield Ok(SseEvent::default().event("end").data("{}"));
    };

    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(60)))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{Capability, SchemaSource};
    use crate::dispatch::DispatchError;
    use async_trait::async_trait;
    use ferridis_core::{IntentVerb, Manifest};

    const VALID_MANIFEST: &str = r#"{
        "ferridis_version": "0.1",
        "id": "test.echo.v1",
        "name": "Echo",
        "category": "test",
        "summary": "Echo whatever you send.",
        "intents": ["echo"],
        "schema": { "type": "openapi-3", "url": "https://example.invalid/s.yaml" },
        "tiers": ["native"],
        "auth": { "type": "none" }
    }"#;

    struct Echo {
        manifest: Manifest,
    }

    #[async_trait]
    impl Capability for Echo {
        fn manifest(&self) -> &Manifest {
            &self.manifest
        }

        fn schema(&self) -> SchemaSource {
            SchemaSource::Embedded {
                content_type: "application/yaml".into(),
                body: "openapi: 3.0.0\n".into(),
            }
        }

        async fn dispatch(
            &self,
            intent: &IntentVerb,
            body: serde_json::Value,
        ) -> Result<serde_json::Value, DispatchError> {
            if intent.as_str() != "echo" {
                return Err(DispatchError::UnsupportedIntent(intent.clone()));
            }
            Ok(body)
        }
    }

    fn echo_server() -> AdapterServer {
        AdapterServer::new(Echo {
            manifest: Manifest::parse(VALID_MANIFEST).unwrap(),
        })
    }

    async fn spawn(server: AdapterServer) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = server.into_router();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn serves_the_manifest() {
        let addr = spawn(echo_server()).await;
        let url = format!("http://{addr}/manifest.json");
        let body = reqwest::get(&url).await.unwrap().text().await.unwrap();
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["id"], "test.echo.v1");
        assert_eq!(json["intents"][0], "echo");
    }

    /// PTC-1 on the wire: a publisher's `callers` declaration survives the
    /// served manifest and parses back to the same policy, so a client can
    /// act on it. Flat intents stay flat.
    #[tokio::test]
    async fn served_manifest_carries_the_callers_declaration() {
        let declared = VALID_MANIFEST.replace(
            r#""intents": ["echo"]"#,
            r#""intents": [{"verb": "echo", "callers": "code"}, "ping"]"#,
        );
        let addr = spawn(AdapterServer::new(Echo {
            manifest: Manifest::parse(&declared).expect("declared manifest parses"),
        }))
        .await;
        let body = reqwest::get(format!("http://{addr}/manifest.json"))
            .await
            .expect("manifest served")
            .text()
            .await
            .expect("manifest body");
        let served = Manifest::parse(&body).expect("served manifest parses");
        let verb = |s: &str| IntentVerb::parse(s).expect("test verb is canonical");
        assert_eq!(
            served.declared_callers(&verb("echo")),
            Some(ferridis_core::CallerPolicy::Code)
        );
        assert_eq!(served.declared_callers(&verb("ping")), None);
        let json: serde_json::Value = serde_json::from_str(&body).expect("served JSON");
        assert_eq!(json["intents"][1], "ping", "undeclared intents stay flat");
    }

    #[tokio::test]
    async fn serves_an_embedded_schema() {
        let addr = spawn(echo_server()).await;
        let url = format!("http://{addr}/schema");
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(
            resp.headers()["content-type"].to_str().unwrap(),
            "application/yaml"
        );
        assert!(resp.text().await.unwrap().contains("openapi"));
    }

    #[tokio::test]
    async fn dispatches_a_declared_intent() {
        let addr = spawn(echo_server()).await;
        let url = format!("http://{addr}/intents/echo");
        let resp = reqwest::Client::new()
            .post(&url)
            .json(&serde_json::json!({"hello": "world"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["hello"], "world");
    }

    #[tokio::test]
    async fn rejects_an_undeclared_intent() {
        let addr = spawn(echo_server()).await;
        // `read-file` is not in the echo manifest's intent list, so the
        // route is never mounted: axum returns 404.
        let url = format!("http://{addr}/intents/read-file");
        let resp = reqwest::Client::new()
            .post(&url)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }
}
