//! End-to-end integration tests for `ferridis-adapter-github`.
//!
//! Architecture: each test spins a wiremock server that stands in for the
//! GitHub API, then starts a real `AdapterServer` on a random port, then
//! drives it with `reqwest` — exactly as a Ferridis client would.

use std::net::SocketAddr;

use base64::Engine as _;
use ferridis_adapter_github::{GitHubCapability, GitHubToken};
use ferridis_adapter_sdk::AdapterServer;
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use wiremock::matchers::{method, path, path_regex, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

/// Spin an adapter server pointing its GitHub client at `github_mock`.
/// Returns the bound socket address.
async fn start_adapter(github_mock: &MockServer) -> SocketAddr {
    let token = GitHubToken::parse("ghp_test_token").expect("valid test token");
    let cap = GitHubCapability::new(token)
        .expect("capability build")
        .with_api_base_url(github_mock.uri());

    let server = AdapterServer::new(cap);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap(); // allow:unwrap — OS assigns port
    let addr = listener.local_addr().unwrap(); // allow:unwrap — just bound
    let router = server.into_router();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap_or_default() // allow:unwrap — test server drop is fine
    });
    addr
}

fn http() -> reqwest::Client {
    reqwest::Client::new()
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn manifest_is_served() {
    let github = MockServer::start().await;
    let addr = start_adapter(&github).await;

    let res = http()
        .get(format!("http://{addr}/manifest.json"))
        .send()
        .await
        .expect("GET /manifest.json");

    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_eq!(body["id"], "ferridis.github.v1");
    assert!(body["intents"].as_array().is_some_and(|a| a.len() == 6));
}

#[tokio::test]
async fn list_repos_returns_repo_list() {
    let github = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/users/octocat/repos"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "id": 1, "name": "hello-world", "private": false },
            { "id": 2, "name": "Spoon-Knife",  "private": false },
        ])))
        .mount(&github)
        .await;

    let addr = start_adapter(&github).await;
    let res = http()
        .post(format!("http://{addr}/intents/list-repos"))
        .json(&json!({ "owner": "octocat" }))
        .send()
        .await
        .expect("POST list-repos");

    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "list-repos", &body).await;
    assert_eq!(body.as_array().expect("array").len(), 2);
    assert_eq!(body[0]["name"], "hello-world");
}

#[tokio::test]
async fn get_file_decodes_base64_content() {
    let content = "Hello, Ferridis!";
    let encoded = base64::engine::general_purpose::STANDARD.encode(content);

    let github = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/octocat/hello-world/contents/README.md"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "path": "README.md",
            "sha":  "abc123",
            "size": content.len(),
            "content": encoded,
        })))
        .mount(&github)
        .await;

    let addr = start_adapter(&github).await;
    let res = http()
        .post(format!("http://{addr}/intents/get-file"))
        .json(&json!({ "owner": "octocat", "repo": "hello-world", "path": "README.md" }))
        .send()
        .await
        .expect("POST get-file");

    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "get-file", &body).await;
    assert_eq!(body["content"], content);
    assert_eq!(body["sha"], "abc123");
}

#[tokio::test]
async fn list_issues_returns_issue_list() {
    let github = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r"/repos/octocat/hello-world/issues"))
        .and(query_param("state", "open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "number": 1, "title": "Bug report",     "state": "open" },
            { "number": 2, "title": "Feature request", "state": "open" },
        ])))
        .mount(&github)
        .await;

    let addr = start_adapter(&github).await;
    let res = http()
        .post(format!("http://{addr}/intents/list-issues"))
        .json(&json!({ "owner": "octocat", "repo": "hello-world", "state": "open" }))
        .send()
        .await
        .expect("POST list-issues");

    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "list-issues", &body).await;
    let issues = body.as_array().expect("array");
    assert_eq!(issues.len(), 2);
    assert_eq!(issues[0]["title"], "Bug report");
}

#[tokio::test]
async fn create_issue_returns_created_issue() {
    let github = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/octocat/hello-world/issues"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "number": 42,
            "title":  "My test issue",
            "state":  "open",
            "html_url": "https://github.com/octocat/hello-world/issues/42",
        })))
        .mount(&github)
        .await;

    let addr = start_adapter(&github).await;
    let res = http()
        .post(format!("http://{addr}/intents/create-issue"))
        .json(&json!({
            "owner": "octocat",
            "repo":  "hello-world",
            "title": "My test issue",
            "body":  "Created via Ferridis."
        }))
        .send()
        .await
        .expect("POST create-issue");

    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "create-issue", &body).await;
    assert_eq!(body["number"], 42);
    assert_eq!(body["title"], "My test issue");
}

#[tokio::test]
async fn list_pull_requests_returns_pr_list() {
    let github = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path_regex(r"/repos/octocat/hello-world/pulls"))
        .and(query_param("state", "open"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            { "number": 10, "title": "Add feature", "state": "open" },
        ])))
        .mount(&github)
        .await;

    let addr = start_adapter(&github).await;
    let res = http()
        .post(format!("http://{addr}/intents/list-pull-requests"))
        .json(&json!({ "owner": "octocat", "repo": "hello-world" }))
        .send()
        .await
        .expect("POST list-pull-requests");

    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "list-pull-requests", &body).await;
    let prs = body.as_array().expect("array");
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0]["number"], 10);
}

#[tokio::test]
async fn search_code_returns_results() {
    let github = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search/code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "total_count": 1,
            "items": [
                {
                    "name": "lib.rs",
                    "path": "src/lib.rs",
                    "repository": { "full_name": "octocat/hello-world" }
                }
            ]
        })))
        .mount(&github)
        .await;

    let addr = start_adapter(&github).await;
    let res = http()
        .post(format!("http://{addr}/intents/search-code"))
        .json(&json!({ "query": "fn main repo:octocat/hello-world" }))
        .send()
        .await
        .expect("POST search-code");

    assert_eq!(res.status(), StatusCode::OK);
    let body: Value = res.json().await.expect("JSON");
    assert_conforms(addr, "search-code", &body).await;
    assert_eq!(body["total_count"], 1);
    assert_eq!(body["items"][0]["name"], "lib.rs");
}

#[tokio::test]
async fn unknown_intent_returns_404() {
    let github = MockServer::start().await;
    let addr = start_adapter(&github).await;

    let res = http()
        .post(format!("http://{addr}/intents/delete-everything"))
        .json(&json!({}))
        .send()
        .await
        .expect("POST unknown-intent");

    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}
