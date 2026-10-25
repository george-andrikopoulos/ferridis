//! End-to-end integration tests for `ferridis-adapter-notion`.

use ferridis_adapter_notion::{IntegrationToken, NotionCapability};
use ferridis_adapter_sdk::AdapterServer;
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn start_adapter(notion_mock: &MockServer) -> SocketAddr {
    let token = IntegrationToken::parse("secret_test").expect("valid token");
    let cap = NotionCapability::new(token)
        .expect("capability")
        .with_api_base_url(notion_mock.uri());
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
    let notion = MockServer::start().await;
    let addr = start_adapter(&notion).await;
    let res = http()
        .get(format!("http://{addr}/manifest.json"))
        .send()
        .await
        .expect("GET manifest");
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_eq!(body["id"], "ferridis.notion.v1");
    assert!(body["intents"].as_array().is_some_and(|a| a.len() == 6));
}

#[tokio::test]
async fn list_databases_returns_results() {
    let notion = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "results": [{ "object": "database", "id": "db001" }],
            "has_more": false
        })))
        .mount(&notion)
        .await;

    let addr = start_adapter(&notion).await;
    let res = http()
        .post(format!("http://{addr}/intents/list-databases"))
        .json(&json!({}))
        .send()
        .await
        .expect("list-databases");
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "list-databases", &body).await;
    assert_eq!(body["results"].as_array().expect("array").len(), 1);
}

#[tokio::test]
async fn query_database_returns_pages() {
    let notion = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/databases/db001/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "results": [{ "object": "page", "id": "page001" }],
            "has_more": false
        })))
        .mount(&notion)
        .await;

    let addr = start_adapter(&notion).await;
    let res = http()
        .post(format!("http://{addr}/intents/query-database"))
        .json(&json!({ "database_id": "db001" }))
        .send()
        .await
        .expect("query-database");
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "query-database", &body).await;
    assert_eq!(body["results"].as_array().expect("array").len(), 1);
}

#[tokio::test]
async fn get_page_returns_page() {
    let notion = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/pages/page001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "page", "id": "page001", "properties": {}
        })))
        .mount(&notion)
        .await;

    let addr = start_adapter(&notion).await;
    let res = http()
        .post(format!("http://{addr}/intents/get-page"))
        .json(&json!({ "page_id": "page001" }))
        .send()
        .await
        .expect("get-page");
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "get-page", &body).await;
    assert_eq!(body["id"], "page001");
}

#[tokio::test]
async fn create_page_returns_page() {
    let notion = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/pages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "page", "id": "page002", "properties": {}
        })))
        .mount(&notion)
        .await;

    let addr = start_adapter(&notion).await;
    let res = http()
        .post(format!("http://{addr}/intents/create-page"))
        .json(&json!({
            "parent": { "database_id": "db001" },
            "properties": { "Name": { "title": [{ "text": { "content": "New Page" } }] } }
        }))
        .send()
        .await
        .expect("create-page");
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "create-page", &body).await;
    assert_eq!(body["id"], "page002");
}

#[tokio::test]
async fn update_page_strips_page_id_and_patches() {
    let notion = MockServer::start().await;
    Mock::given(method("PATCH"))
        .and(path("/pages/page001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "page", "id": "page001", "archived": true
        })))
        .mount(&notion)
        .await;

    let addr = start_adapter(&notion).await;
    let res = http()
        .post(format!("http://{addr}/intents/update-page"))
        .json(&json!({ "page_id": "page001", "archived": true }))
        .send()
        .await
        .expect("update-page");
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "update-page", &body).await;
    assert_eq!(body["id"], "page001");
    assert_eq!(body["archived"], true);
}

#[tokio::test]
async fn search_returns_results() {
    let notion = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "results": [{ "object": "page", "id": "page001" }],
            "has_more": false
        })))
        .mount(&notion)
        .await;

    let addr = start_adapter(&notion).await;
    let res = http()
        .post(format!("http://{addr}/intents/search"))
        .json(&json!({ "query": "meeting notes" }))
        .send()
        .await
        .expect("search");
    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "search", &body).await;
    assert_eq!(body["results"].as_array().expect("array").len(), 1);
}

#[tokio::test]
async fn unknown_intent_returns_404() {
    let notion = MockServer::start().await;
    let addr = start_adapter(&notion).await;
    let res = http()
        .post(format!("http://{addr}/intents/delete-workspace"))
        .json(&json!({}))
        .send()
        .await
        .expect("unknown");
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn missing_database_id_returns_400() {
    let notion = MockServer::start().await;
    let addr = start_adapter(&notion).await;
    let res = http()
        .post(format!("http://{addr}/intents/query-database"))
        .json(&json!({ "filter": {} }))
        .send()
        .await
        .expect("missing field");
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}
