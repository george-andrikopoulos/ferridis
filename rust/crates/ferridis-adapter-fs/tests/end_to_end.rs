//! End-to-end test for the Ferridis filesystem adapter.
//!
//! Spins the [`AdapterServer`] in a tokio task, fetches the manifest,
//! invokes each of the five intents over real HTTP, and verifies the
//! results. This is the canonical proof point for the Ferridis stack.

use std::net::SocketAddr;

use ferridis_adapter_fs::{FilesystemCapability, Root};
use ferridis_adapter_sdk::AdapterServer;
use tempfile::TempDir;
use tokio::net::TcpListener;

async fn spin(root_dir: &TempDir) -> SocketAddr {
    let root = Root::new(root_dir.path()).unwrap();
    let cap = FilesystemCapability::new(root).unwrap();
    let server = AdapterServer::new(cap);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = server.into_router();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

#[tokio::test]
async fn fetches_a_well_formed_manifest() {
    let dir = TempDir::new().unwrap();
    let addr = spin(&dir).await;
    let body = reqwest::get(format!("http://{addr}/manifest.json"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let manifest = ferridis_core::Manifest::parse(&body)
        .expect("the adapter's manifest must round-trip through the core validator");
    assert_eq!(manifest.id(), "ferridis.fs.v1");
    let intent_strs: Vec<&str> = manifest.intents().iter().map(|i| i.as_str()).collect();
    for expected in [
        "read-file",
        "write-file",
        "list-dir",
        "search-files",
        "move-file",
    ] {
        assert!(
            intent_strs.contains(&expected),
            "missing intent: {expected}"
        );
    }
}

#[tokio::test]
async fn write_then_read_round_trips_a_file() {
    let dir = TempDir::new().unwrap();
    let addr = spin(&dir).await;
    let client = reqwest::Client::new();

    let write_resp = client
        .post(format!("http://{addr}/intents/write-file"))
        .json(&serde_json::json!({"path": "hello.txt", "content": "Hello, Ferridis!"}))
        .send()
        .await
        .unwrap();
    assert_eq!(write_resp.status(), 200);

    let read_resp = client
        .post(format!("http://{addr}/intents/read-file"))
        .json(&serde_json::json!({"path": "hello.txt"}))
        .send()
        .await
        .unwrap();
    assert_eq!(read_resp.status(), 200);
    let body: serde_json::Value = read_resp.json().await.unwrap();
    assert_eq!(body["content"], "Hello, Ferridis!");
}

#[tokio::test]
async fn list_dir_returns_written_files() {
    let dir = TempDir::new().unwrap();
    let addr = spin(&dir).await;
    let client = reqwest::Client::new();

    for name in ["a.txt", "b.txt", "c.txt"] {
        client
            .post(format!("http://{addr}/intents/write-file"))
            .json(&serde_json::json!({"path": name, "content": "x"}))
            .send()
            .await
            .unwrap();
    }

    let resp = client
        .post(format!("http://{addr}/intents/list-dir"))
        .json(&serde_json::json!({"path": ""}))
        .send()
        .await
        .unwrap();
    let entries: Vec<serde_json::Value> = resp.json().await.unwrap();
    let names: Vec<&str> = entries
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"a.txt"));
    assert!(names.contains(&"b.txt"));
    assert!(names.contains(&"c.txt"));
}

#[tokio::test]
async fn search_files_finds_matches() {
    let dir = TempDir::new().unwrap();
    let addr = spin(&dir).await;
    let client = reqwest::Client::new();

    client
        .post(format!("http://{addr}/intents/write-file"))
        .json(&serde_json::json!({"path": "sub/dir/important-notes.md", "content": "x"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("http://{addr}/intents/write-file"))
        .json(&serde_json::json!({"path": "junk.txt", "content": "x"}))
        .send()
        .await
        .unwrap();

    let resp = client
        .post(format!("http://{addr}/intents/search-files"))
        .json(&serde_json::json!({"path": "", "name_contains": "important"}))
        .send()
        .await
        .unwrap();
    let hits: Vec<String> = resp.json().await.unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].ends_with("important-notes.md"));
}

#[tokio::test]
async fn move_file_relocates_a_file() {
    let dir = TempDir::new().unwrap();
    let addr = spin(&dir).await;
    let client = reqwest::Client::new();

    client
        .post(format!("http://{addr}/intents/write-file"))
        .json(&serde_json::json!({"path": "orig.txt", "content": "stays"}))
        .send()
        .await
        .unwrap();

    let resp = client
        .post(format!("http://{addr}/intents/move-file"))
        .json(&serde_json::json!({"from": "orig.txt", "to": "moved/here.txt"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let read = client
        .post(format!("http://{addr}/intents/read-file"))
        .json(&serde_json::json!({"path": "moved/here.txt"}))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = read.json().await.unwrap();
    assert_eq!(body["content"], "stays");
}

/// Fetch the adapter's served `/schema` and assert `body` (the real
/// response of `intent`) validates against the response schema it declares.
async fn assert_conforms(addr: SocketAddr, intent: &str, body: &serde_json::Value) {
    let doc = reqwest::get(format!("http://{addr}/schema"))
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

/// Every intent's real response matches the response schema the adapter
/// serves. This is what makes the schema appended to tool descriptions
/// (`ferridis-client::catalogue`) true rather than aspirational.
#[tokio::test]
async fn every_intent_response_conforms_to_the_served_schema() {
    let dir = TempDir::new().unwrap();
    let addr = spin(&dir).await;
    let client = reqwest::Client::new();
    let calls = [
        (
            "write-file",
            serde_json::json!({"path": "sub/notes.md", "content": "x"}),
        ),
        ("read-file", serde_json::json!({"path": "sub/notes.md"})),
        ("list-dir", serde_json::json!({"path": ""})),
        (
            "search-files",
            serde_json::json!({"path": "", "name_contains": "notes"}),
        ),
        (
            "move-file",
            serde_json::json!({"from": "sub/notes.md", "to": "moved.md"}),
        ),
    ];
    for (intent, args) in calls {
        let resp = client
            .post(format!("http://{addr}/intents/{intent}"))
            .json(&args)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{intent}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_conforms(addr, intent, &body).await;
    }
}

#[tokio::test]
async fn refuses_path_traversal_escape() {
    let dir = TempDir::new().unwrap();
    let addr = spin(&dir).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("http://{addr}/intents/read-file"))
        .json(&serde_json::json!({"path": "../../etc/passwd"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    let msg = body["error"].as_str().unwrap();
    assert!(msg.contains("escapes"), "unexpected error: {msg}");
}

#[tokio::test]
async fn undeclared_intent_returns_404() {
    let dir = TempDir::new().unwrap();
    let addr = spin(&dir).await;
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://{addr}/intents/send-message"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}
