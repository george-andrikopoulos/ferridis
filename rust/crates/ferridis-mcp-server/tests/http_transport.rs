//! End-to-end test for the HTTP transport (claude.ai-Connectors shape).
//!
//! Spawns the compiled `ferridis-mcp-server` binary in `--http-bind`
//! mode against an in-test stream-kind adapter, then drives MCP over
//! `POST /mcp` with bearer auth. Covers:
//!
//! - Missing / wrong bearer → 401.
//! - `initialize` → 200 with the standard handshake body.
//! - `notifications/initialized` → 202 Accepted.
//! - `tools/list` → the in-test capability's intent.
//! - `tools/call` against a stream-kind intent → the collected
//!   `result` + `chunks` shape (same path the stdio test
//!   `tools_call_handles_stream_kind_intents` covers).
//! - Non-JSON body → JSON-RPC `parse_error` envelope on a 200.

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use ferridis_adapter_sdk::{AdapterServer, Capability, DispatchError, IntentStream, SchemaSource};
use ferridis_core::{IntentVerb, Manifest};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};

const MANIFEST: &str = r##"{
    "ferridis_version": "0.3",
    "id": "test.httpy.v1",
    "name": "httpy",
    "category": "test",
    "summary": "stream-kind test capability for the HTTP transport test.",
    "intents": [
        {"verb": "say-hi", "kind": "stream", "chunk_schema_url": "https://x/chunk.json"}
    ],
    "schema": { "type": "openapi-3", "url": "https://x/s.yaml" },
    "tiers": ["native"],
    "auth": { "type": "none" }
}"##;

struct Httpy {
    manifest: Manifest,
}

#[async_trait]
impl Capability for Httpy {
    fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    fn schema(&self) -> SchemaSource {
        SchemaSource::Embedded {
            content_type: "application/yaml".into(),
            body: "openapi: 3.0.0\n".into(),
        }
    }
    async fn dispatch(&self, intent: &IntentVerb, _body: Value) -> Result<Value, DispatchError> {
        Err(DispatchError::UnsupportedIntent(intent.clone()))
    }
    async fn dispatch_stream(
        &self,
        _intent: &IntentVerb,
        _body: Value,
    ) -> Result<IntentStream, DispatchError> {
        let s = async_stream::stream! {
            yield Ok(json!({"type":"system","subtype":"init"}));
            yield Ok(json!({
                "type":"assistant",
                "message":{"content":[{"type":"text","text":"PONG"}]}
            }));
            yield Ok(json!({
                "type":"result","subtype":"success","result":"PONG"
            }));
        };
        Ok(Box::pin(s))
    }
}

fn binary_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_ferridis-mcp-server"))
}

async fn spin_httpy_adapter() -> SocketAddr {
    let cap = Httpy {
        manifest: Manifest::parse(MANIFEST).expect("manifest parses"),
    };
    let server = AdapterServer::new(cap);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = server.into_router();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

fn ephemeral_port() -> u16 {
    // Bind then immediately release. Race-free in practice because
    // tokio's reuse-time on the loopback is well under the spawn
    // budget we have between this call and the publisher binding.
    let l = StdTcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

fn write_adapters_config(dir: &TempDir, adapter_addr: SocketAddr) -> std::path::PathBuf {
    let path = dir.path().join("adapters.json");
    let body = json!([{
        "capability": "ferridis://personal.test/httpy/v1@v1",
        "manifestUrl": format!("http://{adapter_addr}/manifest.json"),
        "baseUrl":     format!("http://{adapter_addr}/"),
    }]);
    std::fs::write(&path, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
    path
}

async fn spawn_publisher(cfg_path: &std::path::Path, bind: SocketAddr, bearer: &str) -> Child {
    Command::new(binary_path())
        .arg("--adapters-config")
        .arg(cfg_path)
        .arg("--http-bind")
        .arg(bind.to_string())
        .arg("--bearer-token")
        .arg(bearer)
        .env("FERRIDIS_WALLET_MEMORY", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ferridis-mcp-server --http-bind")
}

async fn wait_until_listening(bind: SocketAddr) {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        if tokio::net::TcpStream::connect(bind).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("publisher never bound {bind}");
}

#[tokio::test]
async fn http_transport_full_handshake_and_stream_call() {
    let work = TempDir::new().unwrap();
    let adapter_addr = spin_httpy_adapter().await;
    let cfg_path = write_adapters_config(&work, adapter_addr);

    let port = ephemeral_port();
    let bind: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let bearer = "test-bearer-abcdef";
    let mut child = spawn_publisher(&cfg_path, bind, bearer).await;
    wait_until_listening(bind).await;

    let client = reqwest::Client::new();
    let url = format!("http://{bind}/mcp");

    // ---- 1. missing bearer → 401 ----
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "missing bearer must 401");

    // ---- 2. wrong bearer → 401 ----
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("authorization", "Bearer wrong-token")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "wrong bearer must 401");

    // From here on, use the correct bearer.
    let auth = format!("Bearer {bearer}");

    // ---- 3. initialize ----
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .json(&json!({
            "jsonrpc":"2.0", "id": 1, "method":"initialize",
            "params": {"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let init: Value = resp.json().await.unwrap();
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(init["result"]["serverInfo"]["name"], "ferridis");

    // ---- 4. notifications/initialized → 202 ----
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .json(&json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202, "notifications must 202");

    // ---- 5. tools/list ----
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .json(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let listed: Value = resp.json().await.unwrap();
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"ferridis_test_httpy_v1_say_hi"),
        "stream-kind intent missing: {names:?}"
    );

    // ---- 6. tools/call against stream-kind intent ----
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .json(&json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"ferridis_test_httpy_v1_say_hi","arguments":{}}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let called: Value = resp.json().await.unwrap();
    assert!(
        called["result"].get("isError").is_none() || called["result"]["isError"] == false,
        "tools/call must succeed: {called}"
    );
    let text = called["result"]["content"][0]["text"]
        .as_str()
        .expect("content text");
    let parsed: Value = serde_json::from_str(text).expect("content text is JSON");
    assert_eq!(parsed["result"], "PONG", "primary text must be PONG");
    let chunks = parsed["chunks"].as_array().expect("chunks");
    assert_eq!(chunks.len(), 3, "all chunks preserved");

    // ---- 7. non-JSON body → parse-error envelope on 200 ----
    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("authorization", &auth)
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["error"]["code"], -32700, "parse error code");

    // Cleanup.
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
}

#[tokio::test]
async fn publisher_refuses_http_bind_without_bearer() {
    // No `--bearer-token`, no `FERRIDIS_MCP_BEARER` → exit 2 (arg
    // parse error). Refusing to start with no auth is the load-bearing
    // safety promise of the HTTP transport.
    let work = TempDir::new().unwrap();
    let adapter_addr = spin_httpy_adapter().await;
    let cfg_path = write_adapters_config(&work, adapter_addr);

    let port = ephemeral_port();
    let bind: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    let output = Command::new(binary_path())
        .arg("--adapters-config")
        .arg(&cfg_path)
        .arg("--http-bind")
        .arg(bind.to_string())
        .env("FERRIDIS_WALLET_MEMORY", "1")
        .env_remove("FERRIDIS_MCP_BEARER")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("spawn ferridis-mcp-server");

    assert!(
        !output.status.success(),
        "publisher must refuse --http-bind with no bearer"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("bearer"),
        "stderr should mention bearer; got: {stderr}"
    );
}

#[tokio::test]
async fn publisher_reads_the_bearer_from_a_file() {
    // The service-manager path: the launch configuration names a file,
    // and the token itself never appears on the command line or in the
    // environment. A trailing CRLF (Notepad, PowerShell `Set-Content`)
    // must not become part of the token.
    let work = TempDir::new().unwrap();
    let adapter_addr = spin_httpy_adapter().await;
    let cfg_path = write_adapters_config(&work, adapter_addr);
    let token_path = work.path().join("bearer.token");
    std::fs::write(&token_path, "file-bearer-xyz\r\n").unwrap();

    let port = ephemeral_port();
    let bind: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let mut child = Command::new(binary_path())
        .arg("--adapters-config")
        .arg(&cfg_path)
        .arg("--http-bind")
        .arg(bind.to_string())
        .arg("--bearer-token-file")
        .arg(&token_path)
        .env("FERRIDIS_WALLET_MEMORY", "1")
        .env_remove("FERRIDIS_MCP_BEARER")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn ferridis-mcp-server --bearer-token-file");
    wait_until_listening(bind).await;

    let client = reqwest::Client::new();
    let url = format!("http://{bind}/mcp");
    let ping = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;

    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .body(ping)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "no bearer must 401");

    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("authorization", "Bearer file-bearer-xyz")
        .body(ping)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "the token from the file must authorize");

    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
}

#[tokio::test]
async fn publisher_refuses_a_bearer_file_beside_another_bearer_source() {
    // Two sources for one credential is refused rather than ranked: a
    // silent precedence would let a stale env var or argument win over
    // the file the operator thinks is in force. The refusal must not
    // echo either token.
    let work = TempDir::new().unwrap();
    let adapter_addr = spin_httpy_adapter().await;
    let cfg_path = write_adapters_config(&work, adapter_addr);
    let token_path = work.path().join("bearer.token");
    std::fs::write(&token_path, "file-secret-value\n").unwrap();

    for (arg, env) in [
        (Some("inline-secret-value"), None),
        (None, Some("env-secret-value")),
    ] {
        let port = ephemeral_port();
        let mut cmd = Command::new(binary_path());
        cmd.arg("--adapters-config")
            .arg(&cfg_path)
            .arg("--http-bind")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--bearer-token-file")
            .arg(&token_path)
            .env("FERRIDIS_WALLET_MEMORY", "1")
            .env_remove("FERRIDIS_MCP_BEARER")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(inline) = arg {
            cmd.arg("--bearer-token").arg(inline);
        }
        if let Some(value) = env {
            cmd.env("FERRIDIS_MCP_BEARER", value);
        }
        let output = tokio::time::timeout(Duration::from_secs(15), cmd.output())
            .await
            .expect("the publisher must exit, not start serving")
            .expect("spawn ferridis-mcp-server");

        assert!(
            !output.status.success(),
            "two bearer sources must be refused"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Not merely "unknown argument": the refusal must be the
        // two-sources one, or this test would pass on a binary that
        // has no --bearer-token-file at all.
        assert!(
            stderr.contains("one source"),
            "stderr should refuse two bearer sources; got: {stderr}"
        );
        for secret in [
            "inline-secret-value",
            "env-secret-value",
            "file-secret-value",
        ] {
            assert!(!stderr.contains(secret), "stderr echoed a token: {stderr}");
        }
    }
}
