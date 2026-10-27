//! End-to-end tests for `ferridis_ptc_run`, the tool that offers
//! programmatic tool calling to MCP clients.
//!
//! Each test spawns the compiled `ferridis-mcp-server` in `--http-bind`
//! mode, as the Windows service runs it, in front of a real filesystem
//! adapter on a real port. The Messages API is a wiremock server answering
//! in the documented programmatic-tool-calling shapes (the same shapes
//! `ferridis-ptc/tests/replay.rs` uses), reached through `--ptc-endpoint`.
//! No tokens are spent.

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use ferridis_adapter_fs::{FilesystemCapability, Root};
use ferridis_adapter_sdk::AdapterServer;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const PTC_TOOL: &str = "ferridis_ptc_run";
const LIST_DIR: &str = "ferridis_ferridis_fs_v1_list_dir";
const BEARER: &str = "test-bearer-ptc";
const API_KEY: &str = "test-api-key";
const CONTAINER: &str = "container_e2e";
/// Far enough ahead that the server's real clock never reaches it.
const EXPIRES: &str = "2099-01-01T00:00:00Z";

fn binary_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_ferridis-mcp-server"))
}

fn ephemeral_port() -> u16 {
    let l = StdTcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let port = l.local_addr().expect("local addr").port();
    drop(l);
    port
}

/// A real fs adapter over `dir`, which holds `a.txt` and `b.txt`.
async fn spin_fs_adapter(dir: &TempDir) -> SocketAddr {
    std::fs::write(dir.path().join("a.txt"), "alpha").expect("seed a");
    std::fs::write(dir.path().join("b.txt"), "beta").expect("seed b");
    let cap = FilesystemCapability::new(Root::new(dir.path()).expect("absolute root"))
        .expect("default manifest parses");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind adapter");
    let addr = listener.local_addr().expect("adapter addr");
    let router = AdapterServer::new(cap).into_router();
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve adapter");
    });
    addr
}

/// The adapters file in its object form, making `list-dir` code-callable.
fn write_adapters_config(dir: &TempDir, adapter: SocketAddr) -> std::path::PathBuf {
    let path = dir.path().join("adapters.json");
    let doc = json!({
        "adapters": [{
            "capability": "ferridis://public.ferridis.io/ferridis/fs@v1",
            "manifestUrl": format!("http://{adapter}/manifest.json"),
            "baseUrl": format!("http://{adapter}/")
        }],
        "callers": { LIST_DIR: "code" }
    });
    std::fs::write(&path, doc.to_string()).expect("write adapters file");
    path
}

/// Answers the i-th request with the i-th response.
struct Sequence {
    responses: Vec<ResponseTemplate>,
    next: AtomicUsize,
}

impl Respond for Sequence {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let i = self.next.fetch_add(1, Ordering::SeqCst);
        self.responses
            .get(i)
            .cloned() // allow:clone — ResponseTemplate is handed out by value; the sequence keeps its copy
            .unwrap_or_else(|| ResponseTemplate::new(500).set_body_string("replay exhausted"))
    }
}

/// Code calls `list-dir` and the response pauses for its result.
fn paused_for_list_dir() -> Value {
    json!({
        "id": "msg_paused", "type": "message", "role": "assistant", "model": "claude-opus-5-5",
        "content": [
            { "type": "server_tool_use", "id": "srvtoolu_e2e", "name": "code_execution",
              "input": { "code": "print(await list_dir({'path': ''}))" } },
            { "type": "tool_use", "id": "toolu_e2e", "name": LIST_DIR, "input": { "path": "" },
              "caller": { "type": "code_execution_20260120", "tool_id": "srvtoolu_e2e" } }
        ],
        "container": { "id": CONTAINER, "expires_at": EXPIRES },
        "stop_reason": "tool_use",
        "usage": { "input_tokens": 30, "cache_creation_input_tokens": 1200,
                   "cache_read_input_tokens": 0, "output_tokens": 90 }
    })
}

fn finished() -> Value {
    json!({
        "id": "msg_done", "type": "message", "role": "assistant", "model": "claude-opus-5-5",
        "content": [
            { "type": "code_execution_tool_result", "tool_use_id": "srvtoolu_e2e",
              "content": { "type": "code_execution_result", "stdout": "a.txt b.txt\n",
                           "stderr": "", "return_code": 0, "content": [] } },
            { "type": "text", "text": "There are two files: a.txt and b.txt." }
        ],
        "container": { "id": CONTAINER, "expires_at": EXPIRES },
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 20, "cache_creation_input_tokens": 150,
                   "cache_read_input_tokens": 1200, "output_tokens": 15 }
    })
}

async fn messages_api(responses: Vec<Value>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(Sequence {
            responses: responses
                .iter()
                .map(|b| ResponseTemplate::new(200).set_body_json(b))
                .collect(),
            next: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;
    server
}

fn spawn_publisher(adapters: &std::path::Path, bind: SocketAddr, ptc_args: &[String]) -> Child {
    Command::new(binary_path())
        .arg("--adapters-config")
        .arg(adapters)
        .arg("--http-bind")
        .arg(bind.to_string())
        .arg("--bearer-token")
        .arg(BEARER)
        .args(ptc_args)
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

/// One JSON-RPC request with the bearer; returns the response body.
async fn rpc(bind: SocketAddr, body: Value) -> Value {
    let resp = reqwest::Client::new()
        .post(format!("http://{bind}/mcp"))
        .header("authorization", format!("Bearer {BEARER}"))
        .json(&body)
        .send()
        .await
        .expect("POST /mcp");
    assert_eq!(resp.status(), 200, "{body}");
    resp.json().await.expect("JSON-RPC response")
}

async fn listed_tools(bind: SocketAddr) -> Vec<Value> {
    rpc(
        bind,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": { "name": "t", "version": "0" } } }),
    )
    .await;
    let listed = rpc(
        bind,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    )
    .await;
    listed["result"]["tools"]
        .as_array()
        .expect("tools array")
        .clone() // allow:clone — the response value is dropped here; the caller owns the list
}

#[tokio::test]
async fn the_tool_is_not_listed_without_an_api_key_file() {
    let work = TempDir::new().expect("tempdir");
    let root = TempDir::new().expect("adapter root");
    let adapter = spin_fs_adapter(&root).await;
    let adapters = write_adapters_config(&work, adapter);
    let bind: SocketAddr = format!("127.0.0.1:{}", ephemeral_port())
        .parse()
        .expect("socket addr");
    let _child = spawn_publisher(&adapters, bind, &[]);
    wait_until_listening(bind).await;

    let tools = listed_tools(bind).await;
    assert!(
        tools.iter().any(|t| t["name"] == LIST_DIR),
        "adapter tools are listed: {tools:?}"
    );
    assert!(
        !tools.iter().any(|t| t["name"] == PTC_TOOL),
        "no key file, so no programmatic-run tool: {tools:?}"
    );
}

/// The wired path: an MCP client calls `ferridis_ptc_run`; the server
/// sends the conversation to the Messages API with the key from the file;
/// the model's code calls `list-dir`, which the server dispatches to the
/// real adapter; the listing goes back as the tool result; and the client
/// gets the final answer with a summary of the run.
#[tokio::test]
async fn a_programmatic_run_reaches_the_published_adapter() {
    let work = TempDir::new().expect("tempdir");
    let root = TempDir::new().expect("adapter root");
    let adapter = spin_fs_adapter(&root).await;
    let adapters = write_adapters_config(&work, adapter);
    let key_file = work.path().join("anthropic.key");
    std::fs::write(&key_file, format!("{API_KEY}\r\n")).expect("write key file");
    let api = messages_api(vec![paused_for_list_dir(), finished()]).await;

    let bind: SocketAddr = format!("127.0.0.1:{}", ephemeral_port())
        .parse()
        .expect("socket addr");
    let ptc_args = vec![
        "--ptc-api-key-file".to_owned(),
        key_file.display().to_string(),
        "--ptc-endpoint".to_owned(),
        format!("{}/v1/messages", api.uri()),
    ];
    let _child = spawn_publisher(&adapters, bind, &ptc_args);
    wait_until_listening(bind).await;

    let tools = listed_tools(bind).await;
    let ptc = tools
        .iter()
        .find(|t| t["name"] == PTC_TOOL)
        .unwrap_or_else(|| panic!("{PTC_TOOL} not listed: {tools:?}"));
    let description = ptc["description"].as_str().expect("description");
    assert!(
        description.contains("1 of this server's tools from code"),
        "the callers map made one tool code-callable: {description}"
    );
    assert_eq!(ptc["inputSchema"]["required"], json!(["task"]));

    let called = rpc(
        bind,
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
            "name": PTC_TOOL, "arguments": { "task": "How many files are there?" } } }),
    )
    .await;
    let result = &called["result"];
    assert!(
        result.get("isError").is_none(),
        "run must succeed: {called}"
    );
    let text = result["content"][0]["text"].as_str().expect("content text");
    assert!(
        text.starts_with("There are two files: a.txt and b.txt."),
        "{text}"
    );
    assert!(
        text.contains("2 turns; 1 calls from code, 0 direct, 0 refused, 0 failed"),
        "{text}"
    );
    assert!(
        text.contains("input tokens 50 uncached, 1350 cache write, 1200 cache read"),
        "{text}"
    );

    let requests = api.received_requests().await.expect("recording is on");
    assert_eq!(requests.len(), 2);
    for r in &requests {
        assert_eq!(
            r.headers.get("x-api-key").map(|v| v.to_str().ok()),
            Some(Some(API_KEY)),
            "the key comes from the file, trimmed"
        );
    }
    let second: Value = serde_json::from_slice(&requests[1].body).expect("JSON body");
    let results = second["messages"]
        .as_array()
        .and_then(|m| m.last())
        .expect("a continuation")["content"]
        .as_array()
        .expect("tool results")
        .clone(); // allow:clone — asserted after the borrow of `second` ends
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["tool_use_id"], "toolu_e2e");
    let listing = results[0]["content"].as_str().expect("string content");
    assert!(
        listing.contains("a.txt") && listing.contains("b.txt"),
        "the real adapter answered: {listing}"
    );
}

#[tokio::test]
async fn ptc_flags_without_a_key_file_are_refused() {
    let work = TempDir::new().expect("tempdir");
    let adapters = work.path().join("adapters.json");
    std::fs::write(&adapters, "[]").expect("write adapters file");
    for flags in [
        ["--ptc-workspace-id", "wrkspc_x"],
        ["--ptc-endpoint", "http://127.0.0.1:9/v1/messages"],
    ] {
        let output = Command::new(binary_path())
            .arg("--adapters-config")
            .arg(&adapters)
            .args(flags)
            .env("FERRIDIS_WALLET_MEMORY", "1")
            .stdin(Stdio::null())
            .output()
            .await
            .expect("run ferridis-mcp-server");
        assert_eq!(output.status.code(), Some(2), "{flags:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("only meaningful with --ptc-api-key-file"),
            "{flags:?}: {stderr}"
        );
    }
}
