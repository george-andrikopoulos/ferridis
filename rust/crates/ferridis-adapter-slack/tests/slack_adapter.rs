//! End-to-end integration tests for `ferridis-adapter-slack`.

use ferridis_adapter_sdk::AdapterServer;
use ferridis_adapter_slack::{BotToken, SlackCapability};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn start_adapter(slack_mock: &MockServer) -> SocketAddr {
    let token = BotToken::parse("xoxb-test").expect("valid token");
    let cap = SlackCapability::new(token)
        .expect("capability")
        .with_api_base_url(slack_mock.uri());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap(); // allow:unwrap test-only
    let addr = listener.local_addr().unwrap(); // allow:unwrap test-only
    tokio::spawn(async move {
        axum::serve(listener, AdapterServer::new(cap).into_router())
            .await
            .unwrap_or_default()
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

#[tokio::test]
async fn manifest_is_served() {
    let slack = MockServer::start().await;
    let addr = start_adapter(&slack).await;
    let res = http()
        .get(format!("http://{addr}/manifest.json"))
        .send()
        .await
        .expect("GET manifest");
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_eq!(body["id"], "ferridis.slack.v1");
    assert!(body["intents"].as_array().is_some_and(|a| a.len() == 6));
}

#[tokio::test]
async fn list_channels_returns_channel_list() {
    let slack = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/conversations.list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "channels": [
                { "id": "C001", "name": "general" },
                { "id": "C002", "name": "random" },
            ]
        })))
        .mount(&slack)
        .await;

    let addr = start_adapter(&slack).await;
    let res = http()
        .post(format!("http://{addr}/intents/list-channels"))
        .json(&json!({}))
        .send()
        .await
        .expect("list-channels");
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "list-channels", &body).await;
    assert_eq!(body["channels"].as_array().expect("array").len(), 2);
}

#[tokio::test]
async fn post_message_returns_message() {
    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat.postMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "ts": "1234567890.000001",
            "channel": "C001",
            "message": { "text": "Hello!" }
        })))
        .mount(&slack)
        .await;

    let addr = start_adapter(&slack).await;
    let res = http()
        .post(format!("http://{addr}/intents/post-message"))
        .json(&json!({ "channel": "C001", "text": "Hello!" }))
        .send()
        .await
        .expect("post-message");
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "post-message", &body).await;
    assert_eq!(body["ok"], true);
    assert_eq!(body["channel"], "C001");
}

#[tokio::test]
async fn get_messages_passes_channel_param() {
    let slack = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/conversations.history"))
        .and(query_param("channel", "C001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "messages": [{ "ts": "123", "text": "Hi" }]
        })))
        .mount(&slack)
        .await;

    let addr = start_adapter(&slack).await;
    let res = http()
        .post(format!("http://{addr}/intents/get-messages"))
        .json(&json!({ "channel": "C001" }))
        .send()
        .await
        .expect("get-messages");
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "get-messages", &body).await;
    assert_eq!(body["messages"].as_array().expect("array").len(), 1);
}

#[tokio::test]
async fn send_dm_opens_channel_then_posts() {
    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/conversations.open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "channel": { "id": "D001" }
        })))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat.postMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "ts": "999", "channel": "D001"
        })))
        .mount(&slack)
        .await;

    let addr = start_adapter(&slack).await;
    let res = http()
        .post(format!("http://{addr}/intents/send-dm"))
        .json(&json!({ "user_id": "U001", "text": "Hey!" }))
        .send()
        .await
        .expect("send-dm");
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "send-dm", &body).await;
    assert_eq!(body["channel"], "D001");
}

#[tokio::test]
async fn get_channel_info_returns_info() {
    let slack = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/conversations.info"))
        .and(query_param("channel", "C001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "channel": { "id": "C001", "name": "general", "is_channel": true }
        })))
        .mount(&slack)
        .await;

    let addr = start_adapter(&slack).await;
    let res = http()
        .post(format!("http://{addr}/intents/get-channel-info"))
        .json(&json!({ "channel": "C001" }))
        .send()
        .await
        .expect("get-channel-info");
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "get-channel-info", &body).await;
    assert_eq!(body["channel"]["name"], "general");
}

#[tokio::test]
async fn list_users_returns_members() {
    let slack = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/users.list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true,
            "members": [{ "id": "U001", "name": "alice" }]
        })))
        .mount(&slack)
        .await;

    let addr = start_adapter(&slack).await;
    let res = http()
        .post(format!("http://{addr}/intents/list-users"))
        .json(&json!({}))
        .send()
        .await
        .expect("list-users");
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "list-users", &body).await;
    assert_eq!(body["members"].as_array().expect("array").len(), 1);
}

#[tokio::test]
async fn unknown_intent_returns_404() {
    let slack = MockServer::start().await;
    let addr = start_adapter(&slack).await;
    let res = http()
        .post(format!("http://{addr}/intents/delete-everything"))
        .json(&json!({}))
        .send()
        .await
        .expect("unknown");
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn missing_channel_returns_400() {
    let slack = MockServer::start().await;
    let addr = start_adapter(&slack).await;
    let res = http()
        .post(format!("http://{addr}/intents/post-message"))
        .json(&json!({ "text": "oops" }))
        .send()
        .await
        .expect("missing field");
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}
