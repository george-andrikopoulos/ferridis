//! End-to-end test exercising the MCP server as a real client would.
//!
//! Spawns the compiled `ferridis-mcp-server` binary, plus a real
//! `ferridis-adapter-fs` on a real port. Drives MCP over stdio:
//! `initialize` → `notifications/initialized` → `tools/list` →
//! `tools/call`. Validates the response shape at each step.

use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;

use ferridis_adapter_fs::{FilesystemCapability, Root};
use ferridis_adapter_sdk::AdapterServer;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::Command;

fn binary_path() -> std::path::PathBuf {
    let raw = env!("CARGO_BIN_EXE_ferridis-mcp-server");
    std::path::PathBuf::from(raw)
}

async fn spin_fs_adapter(dir: &TempDir) -> SocketAddr {
    let root = Root::new(dir.path()).expect("tempdir is absolute existing dir");
    let cap = FilesystemCapability::new(root).expect("default manifest parses");
    let server = AdapterServer::new(cap);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = server.into_router();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    addr
}

fn write_adapters_config(dir: &TempDir, addr: SocketAddr) -> std::path::PathBuf {
    let path = dir.path().join("adapters.json");
    let body = serde_json::json!([{
        "capability": "ferridis://public.ferridis.io/ferridis/fs@v1",
        "manifestUrl": format!("http://{addr}/manifest.json"),
        "baseUrl": format!("http://{addr}/"),
    }]);
    std::fs::write(&path, serde_json::to_vec_pretty(&body).unwrap()).unwrap();
    path
}

struct McpClient {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
}

impl McpClient {
    async fn spawn(adapters_config: &std::path::Path) -> Self {
        // v0.2 wallet is keychain-backed. Tests use the explicit
        // in-memory escape hatch via the env var so they don't depend
        // on the host's keychain (and don't pollute it).
        let mut child = Command::new(binary_path())
            .arg("--adapters-config")
            .arg(adapters_config)
            .env("FERRIDIS_WALLET_MEMORY", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn ferridis-mcp-server");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin,
            stdout,
        }
    }

    async fn send(&mut self, msg: Value) {
        let mut s = msg.to_string();
        s.push('\n');
        self.stdin.write_all(s.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        let mut line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(10), self.stdout.read_line(&mut line))
            .await
            .expect("response timeout")
            .expect("read response");
        assert!(read > 0, "server closed stdout");
        serde_json::from_str(&line).expect("response is JSON")
    }

    async fn shutdown(mut self) {
        drop(self.stdin);
        let _ = tokio::time::timeout(Duration::from_secs(3), self.child.wait()).await;
    }
}

#[tokio::test]
async fn full_mcp_handshake_and_read_file() {
    let work = TempDir::new().unwrap();

    let fs_root = TempDir::new().unwrap();
    std::fs::write(fs_root.path().join("hello.txt"), "hello from MCP shim").unwrap();
    let addr = spin_fs_adapter(&fs_root).await;
    let cfg_path = write_adapters_config(&work, addr);

    let mut mcp = McpClient::spawn(&cfg_path).await;

    // 1. initialize
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test-harness", "version": "0.1.0"}
        }
    }))
    .await;
    let init = mcp.recv().await;
    assert_eq!(init["id"], 1);
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(init["result"]["serverInfo"]["name"], "ferridis");

    // 2. notifications/initialized — no response expected
    mcp.send(json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    }))
    .await;

    // 3. tools/list
    mcp.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}))
        .await;
    let listed = mcp.recv().await;
    let tools = listed["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(
        names.contains(&"ferridis_ferridis_fs_v1_read_file"),
        "missing: {names:?}"
    );
    assert!(names.contains(&"ferridis_ferridis_fs_v1_write_file"));
    assert!(names.contains(&"ferridis_ferridis_fs_v1_list_dir"));

    // 4. tools/call read-file
    mcp.send(json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "ferridis_ferridis_fs_v1_read_file",
            "arguments": {"path": "hello.txt"}
        }
    }))
    .await;
    let called = mcp.recv().await;
    assert_eq!(called["id"], 3);
    let content = called["result"]["content"].as_array().unwrap();
    let text = content[0]["text"].as_str().unwrap();
    assert!(text.contains("hello from MCP shim"), "got: {text}");
    // No `isError` on success.
    assert!(called["result"].get("isError").is_none() || called["result"]["isError"] == false);

    mcp.shutdown().await;
}

#[tokio::test]
async fn tools_call_with_unknown_tool_returns_method_not_found() {
    let work = TempDir::new().unwrap();
    let fs_root = TempDir::new().unwrap();
    let addr = spin_fs_adapter(&fs_root).await;
    let cfg_path = write_adapters_config(&work, addr);

    let mut mcp = McpClient::spawn(&cfg_path).await;
    mcp.send(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name":"t","version":"0"}}
    })).await;
    let _ = mcp.recv().await;
    mcp.send(json!({
        "jsonrpc": "2.0", "method": "notifications/initialized", "params": {}
    }))
    .await;

    mcp.send(json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "nonexistent_tool", "arguments": {}}
    }))
    .await;
    let resp = mcp.recv().await;
    assert_eq!(resp["error"]["code"], -32601);
    mcp.shutdown().await;
}

#[tokio::test]
async fn dispatch_failure_surfaces_as_tool_call_iserror_true() {
    // The fs adapter rejects path traversal with HTTP 400, which the
    // client maps to ProtocolError::BadStatus. The MCP server returns
    // a `tools/call` result with `isError: true` rather than a
    // JSON-RPC error, per the MCP convention.
    let work = TempDir::new().unwrap();
    let fs_root = TempDir::new().unwrap();
    let addr = spin_fs_adapter(&fs_root).await;
    let cfg_path = write_adapters_config(&work, addr);

    let mut mcp = McpClient::spawn(&cfg_path).await;
    mcp.send(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}
    })).await;
    let _ = mcp.recv().await;
    mcp.send(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;

    mcp.send(json!({
        "jsonrpc":"2.0", "id": 2, "method":"tools/call",
        "params":{"name":"ferridis_ferridis_fs_v1_read_file","arguments":{"path":"../../etc/passwd"}}
    })).await;
    let resp = mcp.recv().await;
    assert_eq!(resp["result"]["isError"], true);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Ferridis dispatch failed"), "got: {text}");
    mcp.shutdown().await;
}

#[tokio::test]
async fn rejects_wrong_jsonrpc_version() {
    let work = TempDir::new().unwrap();
    let fs_root = TempDir::new().unwrap();
    let addr = spin_fs_adapter(&fs_root).await;
    let cfg_path = write_adapters_config(&work, addr);

    let mut mcp = McpClient::spawn(&cfg_path).await;
    mcp.send(json!({"jsonrpc":"1.0","id":1,"method":"initialize","params":{}}))
        .await;
    let resp = mcp.recv().await;
    assert_eq!(resp["error"]["code"], -32600);
    mcp.shutdown().await;
}

/// Partial-failure contract: one unreachable Native adapter must not
/// take the publisher down. The successful fs adapter still surfaces
/// in `tools/list`. Regression guard for the pre-fix behaviour where
/// `register_adapters` returned on the first error and `main()`
/// exited with status 1, killing the entire server.
#[tokio::test]
async fn partial_failure_keeps_successful_adapters_alive() {
    let work = TempDir::new().unwrap();

    // Real fs adapter (will register).
    let fs_root = TempDir::new().unwrap();
    std::fs::write(fs_root.path().join("hello.txt"), "still alive").unwrap();
    let good_addr = spin_fs_adapter(&fs_root).await;

    // Grab a port and immediately release it. Subsequent connect()
    // attempts will see ECONNREFUSED — a deterministic register failure.
    let bad_port = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    };

    // adapters.json: one good Native, one bad Native pointing at the
    // freed port. The good one must come second so we also prove the
    // loop continues past the failure.
    let cfg_path = work.path().join("adapters.json");
    let body = serde_json::json!([
        {
            "capability": "ferridis://public.ferridis.io/ferridis/broken@v1",
            "manifestUrl": format!("http://127.0.0.1:{bad_port}/manifest.json"),
            "baseUrl": format!("http://127.0.0.1:{bad_port}/"),
        },
        {
            "capability": "ferridis://public.ferridis.io/ferridis/fs@v1",
            "manifestUrl": format!("http://{good_addr}/manifest.json"),
            "baseUrl": format!("http://{good_addr}/"),
        },
    ]);
    std::fs::write(&cfg_path, serde_json::to_vec_pretty(&body).unwrap()).unwrap();

    let mut mcp = McpClient::spawn(&cfg_path).await;

    // Publisher must respond to initialize — i.e. it did NOT exit on
    // the broken first adapter.
    mcp.send(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name":"t","version":"0"}}
    })).await;
    let init = mcp.recv().await;
    assert_eq!(init["result"]["serverInfo"]["name"], "ferridis");

    mcp.send(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;

    // tools/list must reflect the working subset only — fs intents
    // present, broken adapter's intents absent.
    mcp.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
        .await;
    let listed = mcp.recv().await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"ferridis_ferridis_fs_v1_read_file"),
        "working fs adapter must be present: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("broken")),
        "broken adapter's tools must not be advertised: {names:?}"
    );

    mcp.shutdown().await;
}

/// Edge case: every adapter in the config fails. The publisher must
/// still start and respond to `initialize`, advertising zero tools
/// rather than exiting. Operators see this as ERROR logs + an empty
/// `tools/list`, which is debuggable; a dead binary is not.
#[tokio::test]
async fn partial_failure_with_all_adapters_failing_still_serves() {
    let work = TempDir::new().unwrap();

    let bad_port = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    };

    let cfg_path = work.path().join("adapters.json");
    let body = serde_json::json!([
        {
            "capability": "ferridis://public.ferridis.io/ferridis/broken@v1",
            "manifestUrl": format!("http://127.0.0.1:{bad_port}/manifest.json"),
            "baseUrl": format!("http://127.0.0.1:{bad_port}/"),
        },
    ]);
    std::fs::write(&cfg_path, serde_json::to_vec_pretty(&body).unwrap()).unwrap();

    let mut mcp = McpClient::spawn(&cfg_path).await;
    mcp.send(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name":"t","version":"0"}}
    })).await;
    let init = mcp.recv().await;
    assert_eq!(init["result"]["serverInfo"]["name"], "ferridis");

    mcp.send(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;
    mcp.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
        .await;
    let listed = mcp.recv().await;
    assert_eq!(
        listed["result"]["tools"].as_array().unwrap().len(),
        0,
        "no adapters registered means no tools advertised"
    );

    mcp.shutdown().await;
}

/// Stream-kind dispatch: a capability with a `kind: "stream"` intent
/// must round-trip through `tools/call`. The shim is expected to call
/// `dispatch_streaming`, collect the chunks, extract the final
/// `result.result` text field (the shape the Claude-CLI adapter and
/// the streaming-spec all produce), and return both the primary text
/// and the full chunk array in the response.
///
/// Regression guard for the pre-fix behaviour where stream-kind
/// intents returned `IntentRequiresStreaming` via `client.dispatch`
/// and surfaced as a `tools/call` `isError: true`. That would have
/// silently broken Layer-3-meta-remote-control of Claude Code as
/// soon as anyone added the claude-cli adapter to their config.
#[tokio::test]
async fn tools_call_handles_stream_kind_intents() {
    use async_trait::async_trait;
    use ferridis_adapter_sdk::{Capability, DispatchError, IntentStream, SchemaSource};
    use ferridis_core::{IntentVerb, Manifest};

    const MANIFEST: &str = r##"{
        "ferridis_version": "0.3",
        "id": "test.streamy.v1",
        "name": "streamy",
        "category": "test",
        "summary": "stream-kind test capability.",
        "intents": [
            {"verb": "say-hi", "kind": "stream", "chunk_schema_url": "https://x/chunk.json"}
        ],
        "schema": { "type": "openapi-3", "url": "https://x/s.yaml" },
        "tiers": ["native"],
        "auth": { "type": "none" }
    }"##;

    struct Streamy {
        manifest: Manifest,
    }

    #[async_trait]
    impl Capability for Streamy {
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
            _body: serde_json::Value,
        ) -> Result<serde_json::Value, DispatchError> {
            Err(DispatchError::UnsupportedIntent(intent.clone()))
        }
        async fn dispatch_stream(
            &self,
            _intent: &IntentVerb,
            _body: serde_json::Value,
        ) -> Result<IntentStream, DispatchError> {
            // Mirror the Claude-CLI adapter's wire shape: a couple
            // of intermediate `assistant` chunks plus a terminating
            // `result` chunk carrying the primary text.
            let s = async_stream::stream! {
                yield Ok(serde_json::json!({"type":"system","subtype":"init"}));
                yield Ok(serde_json::json!({
                    "type":"assistant",
                    "message":{"content":[{"type":"text","text":"PONG"}]}
                }));
                yield Ok(serde_json::json!({
                    "type":"result","subtype":"success","result":"PONG"
                }));
            };
            Ok(Box::pin(s))
        }
    }

    // Spin a streamy adapter.
    let cap = Streamy {
        manifest: Manifest::parse(MANIFEST).expect("test manifest parses"),
    };
    let server = AdapterServer::new(cap);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = server.into_router();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    let work = TempDir::new().unwrap();
    let cfg_path = work.path().join("adapters.json");
    let body = serde_json::json!([{
        "capability": "ferridis://personal.test/streamy/v1@v1",
        "manifestUrl": format!("http://{addr}/manifest.json"),
        "baseUrl": format!("http://{addr}/"),
    }]);
    std::fs::write(&cfg_path, serde_json::to_vec_pretty(&body).unwrap()).unwrap();

    let mut mcp = McpClient::spawn(&cfg_path).await;
    mcp.send(json!({
        "jsonrpc":"2.0","id":1,"method":"initialize",
        "params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}
    })).await;
    let _ = mcp.recv().await;
    mcp.send(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;

    mcp.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
        .await;
    let listed = mcp.recv().await;
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"ferridis_test_streamy_v1_say_hi"),
        "stream-kind intent must be advertised: {names:?}"
    );

    mcp.send(json!({
        "jsonrpc":"2.0","id":3,"method":"tools/call",
        "params":{"name":"ferridis_test_streamy_v1_say_hi","arguments":{}}
    }))
    .await;
    let resp = mcp.recv().await;
    // Success — not isError.
    assert!(
        resp["result"].get("isError").is_none() || resp["result"]["isError"] == false,
        "stream-kind tools/call must succeed, got: {resp}"
    );
    // The content text is the JSON-pretty rendering of {result, chunks}.
    let text = resp["result"]["content"][0]["text"]
        .as_str()
        .expect("content text");
    let parsed: serde_json::Value = serde_json::from_str(text).expect("content text is valid JSON");
    assert_eq!(
        parsed["result"], "PONG",
        "primary text must be the result.result"
    );
    let chunks = parsed["chunks"].as_array().expect("chunks array");
    assert_eq!(chunks.len(), 3, "all chunks must be preserved");
    assert_eq!(chunks[0]["type"], "system");
    assert_eq!(chunks[2]["type"], "result");

    mcp.shutdown().await;
}

#[tokio::test]
async fn ping_responds_with_empty_object() {
    let work = TempDir::new().unwrap();
    let fs_root = TempDir::new().unwrap();
    let addr = spin_fs_adapter(&fs_root).await;
    let cfg_path = write_adapters_config(&work, addr);

    let mut mcp = McpClient::spawn(&cfg_path).await;
    mcp.send(json!({"jsonrpc":"2.0","id":1,"method":"ping","params":{}}))
        .await;
    let resp = mcp.recv().await;
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"], serde_json::json!({}));
    mcp.shutdown().await;
}

/// PTC step 2, through the production path: the real binary, a real fs
/// adapter, adapters.json. Every request-kind tool description ends with
/// the response schema the adapter serves at `/schema`, byte for byte as
/// `serde_json` renders it, so a programmatic caller can parse results in
/// code without guessing their shape.
#[tokio::test]
async fn tools_list_descriptions_carry_the_served_response_schemas() {
    let work = TempDir::new().expect("work dir");
    let fs_root = TempDir::new().expect("fs root");
    let addr = spin_fs_adapter(&fs_root).await;
    let cfg_path = write_adapters_config(&work, addr);

    let served: Value = reqwest::get(format!("http://{addr}/schema"))
        .await
        .expect("adapter serves /schema")
        .json()
        .await
        .expect("/schema is JSON");

    let mut mcp = McpClient::spawn(&cfg_path).await;
    mcp.send(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {},
                    "clientInfo": {"name": "test-harness", "version": "0.1.0"} }
    }))
    .await;
    assert_eq!(mcp.recv().await["id"], 1, "initialize must answer first");
    mcp.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}))
        .await;
    let listed = mcp.recv().await;
    let tools = listed["result"]["tools"].as_array().expect("tools array");

    for verb in [
        "read-file",
        "write-file",
        "list-dir",
        "search-files",
        "move-file",
    ] {
        let name = format!("ferridis_ferridis_fs_v1_{}", verb.replace('-', "_"));
        let tool = tools
            .iter()
            .find(|t| t["name"] == name.as_str())
            .unwrap_or_else(|| panic!("{name} missing from tools/list"));
        let description = tool["description"]
            .as_str()
            .expect("description is a string");
        let schema = &served["paths"][format!("/intents/{verb}")]["post"]["responses"]["200"]["content"]
            ["application/json"]["schema"];
        assert!(
            schema.is_object(),
            "fs adapter declares no 200 schema for {verb}"
        );
        assert!(
            description.ends_with(&format!("Returns JSON. Response schema: {schema}")),
            "{name}: {description}"
        );
    }

    mcp.shutdown().await;
}
