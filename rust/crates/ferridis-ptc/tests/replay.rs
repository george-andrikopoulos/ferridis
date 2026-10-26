//! Replay tests: the Messages API is a wiremock server answering with
//! responses in the shapes the programmatic-tool-calling documentation
//! gives (transcribed from the docs, not recorded from a live run), and
//! the tools dispatch to a real filesystem adapter on a real port,
//! registered through the production adapters-file path.
//!
//! The default suite spends no tokens; the live run is in `live_smoke.rs`.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ferridis_adapter_fs::{FilesystemCapability, Root};
use ferridis_adapter_sdk::AdapterServer;
use ferridis_client::Client;
use ferridis_client::adapters::{AdaptersFile, CallerOverrides, register_adapters};
use ferridis_client::catalogue::{Catalogue, CatalogueSource};
use ferridis_core::{CapabilityRef, IntentVerb, Manifest};
use ferridis_ptc::{
    API_VERSION, ApiKey, CODE_EXECUTION_TOOL_TYPE, Clock, Conversation, Executor, MaxTokens,
    MaxTurns, ModelId, PtcClient, PtcError, StopReason, ToolPlan, UserPrompt, WorkspaceId, run,
};
use serde_json::{Value, json};
use tempfile::TempDir;
use time::OffsetDateTime;
use time::macros::datetime;
use tokio::net::TcpListener;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const LIST_DIR: &str = "ferridis_ferridis_fs_v1_list_dir";
const READ_FILE: &str = "ferridis_ferridis_fs_v1_read_file";
const WRITE_FILE: &str = "ferridis_ferridis_fs_v1_write_file";
const CONTAINER: &str = "container_replay";
const EXPIRES: &str = "2026-01-20T14:30:00Z";
const BEFORE_EXPIRY: OffsetDateTime = datetime!(2026-01-20 14:00 UTC);
const AFTER_EXPIRY: OffsetDateTime = datetime!(2026-01-20 14:31 UTC);

#[derive(Debug)]
struct Fixed(OffsetDateTime);

impl Clock for Fixed {
    fn now(&self) -> OffsetDateTime {
        self.0
    }
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
            .cloned() // ResponseTemplate is handed out by value; the sequence keeps its copy
            .unwrap_or_else(|| ResponseTemplate::new(500).set_body_string("replay exhausted"))
    }
}

async fn messages_api(responses: Vec<Value>) -> MockServer {
    messages_api_raw(
        responses
            .iter()
            .map(|b| ResponseTemplate::new(200).set_body_json(b))
            .collect(),
    )
    .await
}

async fn messages_api_raw(responses: Vec<ResponseTemplate>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(Sequence {
            responses,
            next: AtomicUsize::new(0),
        })
        .mount(&server)
        .await;
    server
}

fn ptc_client(server: &MockServer, now: OffsetDateTime) -> PtcClient {
    let endpoint = format!("{}/v1/messages", server.uri())
        .parse()
        .expect("valid url");
    PtcClient::new(ApiKey::new("test-key".to_owned()).expect("non-empty key"))
        .with_endpoint(endpoint)
        .with_clock(Arc::new(Fixed(now)))
}

/// A real fs adapter over a temp dir holding `a.txt` and `b.txt`,
/// registered from an adapters document whose `callers` map makes
/// `list-dir` and `read-file` code-only. Returns the dir (keep it alive),
/// the conversation, and the executor over the client.
async fn fs_session() -> (TempDir, Conversation, Executor<Client>) {
    let dir = TempDir::new().expect("tempdir");
    std::fs::write(dir.path().join("a.txt"), "alpha").expect("seed a");
    std::fs::write(dir.path().join("b.txt"), "beta").expect("seed b");
    let cap = FilesystemCapability::new(Root::new(dir.path()).expect("absolute root"))
        .expect("default manifest parses");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let router = AdapterServer::new(cap).into_router();
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve adapter");
    });

    let doc = json!({
        "adapters": [{
            "capability": "ferridis://public.ferridis.io/ferridis/fs@v1",
            "manifestUrl": format!("http://{addr}/manifest.json"),
            "baseUrl": format!("http://{addr}/")
        }],
        "callers": { LIST_DIR: "code", READ_FILE: "code" }
    });
    let file = AdaptersFile::parse(doc.to_string().as_bytes()).expect("adapters document parses");
    let client = Client::ephemeral();
    let (sources, failures) = register_adapters(&client, file.adapters()).await;
    assert!(failures.is_empty(), "{failures:?}");
    let plan = ToolPlan::build(&Catalogue::from_sources(&sources), file.caller_overrides());
    let conversation = Conversation::new(
        ModelId::default_model(),
        MaxTokens::default(),
        plan.tool_set(),
    );
    (dir, conversation, Executor::new(plan, client))
}

fn prompt() -> UserPrompt {
    UserPrompt::new("List the files and tell me what is in them.").expect("non-empty prompt")
}

/// Docs step 2: the code calls a tool and the response pauses.
fn paused(tool: &str, input: Value) -> Value {
    json!({
        "id": "msg_paused",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-5-5",
        "content": [
            { "type": "text", "text": "I'll query the files from code." },
            { "type": "server_tool_use", "id": "srvtoolu_replay", "name": "code_execution",
              "input": { "code": "files = await list_dir({'path': ''})\nprint(files)" } },
            { "type": "tool_use", "id": "toolu_replay", "name": tool, "input": input,
              "caller": { "type": "code_execution_20260120", "tool_id": "srvtoolu_replay" } }
        ],
        "container": { "id": CONTAINER, "expires_at": EXPIRES },
        "stop_reason": "tool_use",
        "usage": { "input_tokens": 1200, "output_tokens": 180 }
    })
}

/// Docs step 5: the code finished and the turn ends.
fn finished() -> Value {
    json!({
        "id": "msg_done",
        "type": "message",
        "role": "assistant",
        "model": "claude-opus-5-5",
        "content": [
            { "type": "code_execution_tool_result", "tool_use_id": "srvtoolu_replay",
              "content": { "type": "code_execution_result", "stdout": "a.txt b.txt\n",
                           "stderr": "", "return_code": 0, "content": [] } },
            { "type": "text", "text": "There are two files: a.txt and b.txt." }
        ],
        "container": { "id": CONTAINER, "expires_at": EXPIRES },
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 1500, "output_tokens": 40 }
    })
}

async fn bodies(server: &MockServer) -> Vec<(Request, Value)> {
    server
        .received_requests()
        .await
        .expect("request recording is on")
        .into_iter()
        .map(|r| {
            let body = serde_json::from_slice(&r.body).expect("request body is JSON");
            (r, body)
        })
        .collect()
}

fn tool<'v>(tools: &'v Value, name: &str) -> &'v Value {
    tools
        .as_array()
        .expect("tools is an array")
        .iter()
        .find(|t| t["name"] == name)
        .unwrap_or_else(|| panic!("tool {name} not offered: {tools}"))
}

/// The last message of a request: what a continuation adds.
fn last_message(body: &Value) -> &Value {
    body["messages"]
        .as_array()
        .and_then(|m| m.last())
        .expect("request has messages")
}

#[tokio::test]
async fn a_configured_workspace_is_sent_on_every_request() {
    let (_dir, conversation, executor) = fs_session().await;
    let server = messages_api(vec![paused(LIST_DIR, json!({ "path": "" })), finished()]).await;
    let ptc = ptc_client(&server, BEFORE_EXPIRY)
        .with_workspace(WorkspaceId::new("wrkspc_replay").expect("non-empty id"));

    run(&ptc, &executor, conversation, prompt(), MaxTurns::default())
        .await
        .expect("run completes");

    let requests = bodies(&server).await;
    assert_eq!(requests.len(), 2);
    for (req, _) in &requests {
        assert_eq!(
            req.headers
                .get("anthropic-workspace-id")
                .map(|v| v.to_str().ok()),
            Some(Some("wrkspc_replay"))
        );
    }
}

#[tokio::test]
async fn a_programmatic_call_round_trips_through_a_real_adapter() {
    let (_dir, conversation, executor) = fs_session().await;
    let server = messages_api(vec![paused(LIST_DIR, json!({ "path": "" })), finished()]).await;
    let ptc = ptc_client(&server, BEFORE_EXPIRY);

    let outcome = run(&ptc, &executor, conversation, prompt(), MaxTurns::default())
        .await
        .expect("run completes");

    assert_eq!(outcome.final_turn().stop_reason(), &StopReason::EndTurn);
    assert_eq!(
        outcome.final_turn().text(),
        "There are two files: a.txt and b.txt."
    );
    assert_eq!(outcome.turns(), 2);
    assert_eq!(outcome.calls().programmatic(), 1);
    assert_eq!(outcome.calls().refused(), 0);
    assert_eq!(outcome.usage().input_tokens(), 2700);
    assert_eq!(outcome.final_turn().code_runs().len(), 1);

    let requests = bodies(&server).await;
    assert_eq!(requests.len(), 2);
    let (first_req, first) = &requests[0];
    let (_, second) = &requests[1];

    // Headers on the wire.
    assert_eq!(
        first_req.headers.get("x-api-key").map(|v| v.to_str().ok()),
        Some(Some("test-key"))
    );
    assert_eq!(
        first_req
            .headers
            .get("anthropic-version")
            .map(|v| v.to_str().ok()),
        Some(Some(API_VERSION))
    );
    assert!(
        first_req.headers.get("anthropic-workspace-id").is_none(),
        "no workspace configured, so no workspace header"
    );

    // The tools array: code execution offered, policies projected, no
    // strict flag anywhere, and no forced tool choice.
    let tools = &first["tools"];
    assert_eq!(
        tools[0],
        json!({ "type": CODE_EXECUTION_TOOL_TYPE, "name": "code_execution" })
    );
    assert_eq!(
        tool(tools, LIST_DIR)["allowed_callers"],
        json!([CODE_EXECUTION_TOOL_TYPE])
    );
    assert_eq!(
        tool(tools, WRITE_FILE)["allowed_callers"],
        json!(["direct"])
    );
    assert!(!tools.to_string().contains("\"strict\""), "{tools}");
    assert!(first.get("tool_choice").is_none() && first.get("container").is_none());

    // The continuation: same tools, the container named, the assistant
    // content resent verbatim, and a message of tool_results only.
    assert_eq!(
        &second["tools"], tools,
        "tools array must be identical every turn"
    );
    assert_eq!(second["container"], CONTAINER);
    let messages = second["messages"].as_array().expect("messages");
    assert_eq!(
        messages[1]["content"],
        paused(LIST_DIR, json!({ "path": "" }))["content"]
    );
    let results = last_message(second)["content"]
        .as_array()
        .expect("results")
        .clone(); // allow:clone — asserted after the borrow of `second` ends
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["type"], "tool_result");
    assert_eq!(results[0]["tool_use_id"], "toolu_replay");
    assert!(results[0].get("is_error").is_none(), "{results:?}");
    let listing = results[0]["content"].as_str().expect("string content");
    assert!(
        listing.contains("a.txt") && listing.contains("b.txt"),
        "real adapter output: {listing}"
    );
}

/// `write-file` is direct-only (no declaration, no override). Code calling
/// it gets an error result, and the adapter never runs: the file is not
/// written.
#[tokio::test]
async fn a_refused_call_never_reaches_the_adapter() {
    let (dir, conversation, executor) = fs_session().await;
    let input = json!({ "path": "pwned.txt", "content": "should not exist" });
    let server = messages_api(vec![paused(WRITE_FILE, input), finished()]).await;
    let ptc = ptc_client(&server, BEFORE_EXPIRY);

    let outcome = run(&ptc, &executor, conversation, prompt(), MaxTurns::default())
        .await
        .expect("a refusal is a result, not a failure");

    assert!(
        !dir.path().join("pwned.txt").exists(),
        "the adapter ran a refused call"
    );
    assert_eq!(outcome.calls().refused(), 1);
    assert_eq!(outcome.calls().programmatic(), 0);
    let requests = bodies(&server).await;
    let result = &last_message(&requests[1].1)["content"][0];
    assert_eq!(result["is_error"], true);
    assert!(
        result["content"]
            .as_str()
            .is_some_and(|c| c.contains("directly by the model")),
        "{result}"
    );
}

#[tokio::test]
async fn an_expired_container_sends_no_continuation() {
    let (_dir, conversation, executor) = fs_session().await;
    let server = messages_api(vec![paused(LIST_DIR, json!({ "path": "" })), finished()]).await;
    let ptc = ptc_client(&server, AFTER_EXPIRY);

    let err = run(&ptc, &executor, conversation, prompt(), MaxTurns::default())
        .await
        .expect_err("the container expired before the results were ready");

    assert!(
        matches!(&err, PtcError::ContainerExpired { container, .. } if container.as_str() == CONTAINER),
        "{err}"
    );
    assert_eq!(
        bodies(&server).await.len(),
        1,
        "no continuation may be sent"
    );
}

#[tokio::test]
async fn the_turn_limit_stops_a_run_that_keeps_calling() {
    let (_dir, conversation, executor) = fs_session().await;
    let server = messages_api(vec![
        paused(LIST_DIR, json!({ "path": "" })),
        paused(LIST_DIR, json!({ "path": "" })),
        finished(),
    ])
    .await;
    let ptc = ptc_client(&server, BEFORE_EXPIRY);
    let limit = MaxTurns::new(2).expect("non-zero");

    let err = run(&ptc, &executor, conversation, prompt(), limit)
        .await
        .expect_err("two paused turns exhaust a limit of two");
    assert!(matches!(err, PtcError::TurnLimit { limit: 2 }), "{err}");
    assert_eq!(bodies(&server).await.len(), 2);
}

#[tokio::test]
async fn an_api_error_names_its_type_and_message() {
    let (_dir, conversation, executor) = fs_session().await;
    let server = messages_api_raw(vec![ResponseTemplate::new(400).set_body_json(json!({
        "type": "error",
        "error": { "type": "invalid_request_error", "message": "Circular $ref detected" }
    }))])
    .await;
    let ptc = ptc_client(&server, BEFORE_EXPIRY);

    let err = run(&ptc, &executor, conversation, prompt(), MaxTurns::default())
        .await
        .expect_err("400");
    assert!(
        matches!(&err, PtcError::Api { status: 400, error_type, message }
            if error_type == "invalid_request_error" && message == "Circular $ref detected"),
        "{err}"
    );
}

#[derive(Clone, Default)]
struct CaptureBuf(Arc<Mutex<Vec<u8>>>);

impl Write for CaptureBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("capture lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A publisher declares a tool code-callable, but its input schema has a
/// recursive `$ref` the API rejects for programmatic calls: the tool is
/// offered direct-only, the code-execution tool is not offered at all, and
/// a WARN names the tool.
#[test]
fn a_recursive_ref_downgrades_the_tool_with_a_warn_naming_it() {
    let manifest = Manifest::parse(
        r#"{
            "ferridis_version": "0.1", "id": "ferridis.fs.v1", "name": "FS",
            "category": "files", "summary": "Filesystem.",
            "intents": [{ "verb": "list-dir", "callers": "code" }],
            "schema": { "type": "openapi-3", "url": "https://x/s.json" },
            "tiers": ["native"], "auth": { "type": "none" }
        }"#,
    )
    .expect("test manifest parses");
    let recursive = json!({
        "type": "object",
        "properties": { "tree": { "$ref": "#/$defs/node" } },
        "$defs": { "node": { "type": "object",
            "properties": { "children": { "type": "array", "items": { "$ref": "#/$defs/node" } } } } }
    });
    let verb = IntentVerb::parse("list-dir").expect("canonical verb");
    let source = CatalogueSource::new(
        CapabilityRef::parse("ferridis://public.ferridis.io/ferridis/fs@v1").expect("valid ref"),
        manifest,
    )
    .with_input_schemas(Some(Arc::new(HashMap::from([(verb, recursive)]))));

    let capture = CaptureBuf::default();
    let writer = capture.clone(); // clone: the MakeWriter closure needs its own handle to the shared buffer
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone()) // clone: one writer handle per record, same buffer
        .finish();
    let plan = tracing::subscriber::with_default(subscriber, || {
        ToolPlan::build(
            &Catalogue::from_sources(&[source]),
            &CallerOverrides::default(),
        )
    });

    let tools = serde_json::to_value(&*plan.tool_set()).expect("tools serialize");
    assert_eq!(tool(&tools, LIST_DIR)["allowed_callers"], json!(["direct"]));
    assert!(!plan.tool_set().offers_code_execution());
    let logs = String::from_utf8_lossy(&capture.0.lock().expect("capture lock")).into_owned();
    let warn = logs
        .lines()
        .find(|l| l.contains("WARN") && l.contains("downgraded"))
        .unwrap_or_else(|| panic!("no downgrade WARN:\n{logs}"));
    assert!(
        warn.contains(LIST_DIR) && warn.contains("recursive $ref"),
        "{warn}"
    );
}
