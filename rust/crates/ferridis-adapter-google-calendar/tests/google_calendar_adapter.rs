//! End-to-end integration tests for `ferridis-adapter-google-calendar`.
//!
//! Each test spins a wiremock server for the Google Calendar API (and
//! optionally the OAuth token endpoint), starts a real `AdapterServer`
//! on a random port, then drives it with `reqwest` — exactly as a
//! Ferridis client would.

use std::net::SocketAddr;

use ferridis_adapter_google_calendar::{AccessToken, GoogleCalendarCapability};
use ferridis_adapter_sdk::AdapterServer;
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use wiremock::matchers::{header_exists, method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

async fn start_adapter(gcal_mock: &MockServer) -> SocketAddr {
    let token = AccessToken::parse("ya29.test_token").expect("valid test token");
    let cap = GoogleCalendarCapability::new(token)
        .expect("capability build")
        .with_api_base_url(gcal_mock.uri());

    let server = AdapterServer::new(cap);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap(); // allow:unwrap — OS assigns port
    let addr = listener.local_addr().unwrap(); // allow:unwrap — just bound
    let router = server.into_router();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap_or_default() // allow:unwrap — test server drop is fine
    });
    addr
}

/// Fetch the adapter's served `/schema` and assert `body` (the real
/// response of `intent`) validates against the response schema it declares.
async fn assert_conforms(addr: SocketAddr, intent: &str, body: &Value) {
    let doc = http()
        .get(format!("http://{addr}/schema"))
        .send()
        .await
        .expect("GET /schema")
        .bytes()
        .await
        .expect("schema body");
    let verb = ferridis_core::IntentVerb::parse(intent).expect("canonical verb");
    let outcome = ferridis_adapter_sdk::check_response(&doc, &verb, body)
        .expect("served schema is an OpenAPI 3 JSON document");
    assert!(
        outcome.is_conforming(),
        "`{intent}` response does not match its declared schema: {outcome:?}\nresponse: {body}"
    );
}

fn http() -> reqwest::Client {
    reqwest::Client::new()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn manifest_is_served() {
    let gcal = MockServer::start().await;
    let addr = start_adapter(&gcal).await;

    let res = http()
        .get(format!("http://{addr}/manifest.json"))
        .send()
        .await
        .expect("GET /manifest.json");

    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_eq!(body["id"], "ferridis.google-calendar.v1");
    assert!(body["intents"].as_array().is_some_and(|a| a.len() == 6));
}

#[tokio::test]
async fn list_calendars_returns_calendar_list() {
    let gcal = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/users/me/calendarList"))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "kind": "calendar#calendarList",
            "items": [
                { "id": "primary", "summary": "George", "primary": true },
                { "id": "work@example.com", "summary": "Work" },
            ]
        })))
        .mount(&gcal)
        .await;

    let addr = start_adapter(&gcal).await;
    let res = http()
        .post(format!("http://{addr}/intents/list-calendars"))
        .json(&json!({}))
        .send()
        .await
        .expect("POST list-calendars");

    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "list-calendars", &body).await;
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], "primary");
}

#[tokio::test]
async fn list_events_passes_query_params() {
    let gcal = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r"/calendars/primary/events"))
        .and(query_param("maxResults", "5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "kind": "calendar#events",
            "items": [
                { "id": "evt1", "summary": "Stand-up", "status": "confirmed" },
            ]
        })))
        .mount(&gcal)
        .await;

    let addr = start_adapter(&gcal).await;
    let res = http()
        .post(format!("http://{addr}/intents/list-events"))
        .json(&json!({ "calendar_id": "primary", "max_results": 5 }))
        .send()
        .await
        .expect("POST list-events");

    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "list-events", &body).await;
    assert_eq!(body["items"][0]["id"], "evt1");
}

#[tokio::test]
async fn get_event_returns_event() {
    let gcal = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/calendars/primary/events/abc123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "abc123",
            "summary": "Team lunch",
            "status": "confirmed",
        })))
        .mount(&gcal)
        .await;

    let addr = start_adapter(&gcal).await;
    let res = http()
        .post(format!("http://{addr}/intents/get-event"))
        .json(&json!({ "calendar_id": "primary", "event_id": "abc123" }))
        .send()
        .await
        .expect("POST get-event");

    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "get-event", &body).await;
    assert_eq!(body["id"], "abc123");
    assert_eq!(body["summary"], "Team lunch");
}

#[tokio::test]
async fn create_event_strips_routing_fields() {
    let gcal = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/calendars/primary/events"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "new_evt",
            "summary": "Planning meeting",
            "status": "confirmed",
        })))
        .mount(&gcal)
        .await;

    let addr = start_adapter(&gcal).await;
    let res = http()
        .post(format!("http://{addr}/intents/create-event"))
        .json(&json!({
            "calendar_id": "primary",
            "summary": "Planning meeting",
            "start": { "dateTime": "2026-06-01T10:00:00Z" },
            "end": { "dateTime": "2026-06-01T11:00:00Z" },
        }))
        .send()
        .await
        .expect("POST create-event");

    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "create-event", &body).await;
    assert_eq!(body["id"], "new_evt");
    assert_eq!(body["summary"], "Planning meeting");
}

#[tokio::test]
async fn update_event_returns_updated_event() {
    let gcal = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/calendars/primary/events/abc123"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "abc123",
            "summary": "Updated: Team lunch",
            "status": "confirmed",
        })))
        .mount(&gcal)
        .await;

    let addr = start_adapter(&gcal).await;
    let res = http()
        .post(format!("http://{addr}/intents/update-event"))
        .json(&json!({
            "calendar_id": "primary",
            "event_id": "abc123",
            "summary": "Updated: Team lunch",
        }))
        .send()
        .await
        .expect("POST update-event");

    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "update-event", &body).await;
    assert_eq!(body["summary"], "Updated: Team lunch");
}

#[tokio::test]
async fn delete_event_returns_deleted_true() {
    let gcal = MockServer::start().await;
    Mock::given(method("DELETE"))
        .and(path("/calendars/primary/events/abc123"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&gcal)
        .await;

    let addr = start_adapter(&gcal).await;
    let res = http()
        .post(format!("http://{addr}/intents/delete-event"))
        .json(&json!({ "calendar_id": "primary", "event_id": "abc123" }))
        .send()
        .await
        .expect("POST delete-event");

    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "delete-event", &body).await;
    assert_eq!(body["deleted"], true);
}

#[tokio::test]
async fn unknown_intent_returns_404() {
    let gcal = MockServer::start().await;
    let addr = start_adapter(&gcal).await;

    let res = http()
        .post(format!("http://{addr}/intents/delete-everything"))
        .json(&json!({}))
        .send()
        .await
        .expect("POST unknown-intent");

    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn missing_required_field_returns_400() {
    let gcal = MockServer::start().await;
    let addr = start_adapter(&gcal).await;

    let res = http()
        .post(format!("http://{addr}/intents/list-events"))
        .json(&json!({})) // calendar_id missing
        .send()
        .await
        .expect("POST list-events without calendar_id");

    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}
