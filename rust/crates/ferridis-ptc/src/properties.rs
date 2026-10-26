//! Property tests for the three guarantees the typestate and the gate
//! make (step 4 of the PTC brief):
//!
//! 1. every pending call gets exactly one result;
//! 2. a continuation's new message contains only `tool_result` blocks;
//! 3. a call the tool's policy does not allow never reaches a dispatcher.
//!
//! Responses are built in the documented shape and parsed by the
//! production parser, so the properties hold for what the wire delivers,
//! not for hand-built turn values.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use ferridis_client::adapters::CallerOverrides;
use ferridis_client::catalogue::{Catalogue, CatalogueSource};
use ferridis_client::{CallOrigin, ClientError};
use ferridis_core::{CapabilityRef, IntentVerb, Manifest};
use proptest::prelude::*;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::executor::{Dispatcher, Executor};
use crate::request::{Conversation, MaxTokens, ModelId};
use crate::response::{CallHandler, PendingCall, RawResponse, ToolOutput, Turn};
use crate::tools::ToolPlan;

pub(crate) const CODE_TOOL: &str = "ferridis_ferridis_fs_v1_read_file";
const DIRECT_TOOL: &str = "ferridis_ferridis_fs_v1_list_dir";
const UNKNOWN_TOOL: &str = "ferridis_not_registered";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TestCaller {
    Direct,
    Code,
    Unrecognised,
}

impl TestCaller {
    fn json(self) -> Value {
        match self {
            Self::Direct => json!({ "type": "direct" }),
            Self::Code => json!({ "type": "code_execution_20260120", "tool_id": "srvtoolu_p" }),
            Self::Unrecognised => json!({ "type": "mystery_caller_20991231" }),
        }
    }
}

pub(crate) fn plan() -> ToolPlan {
    let manifest = Manifest::parse(
        r#"{
            "ferridis_version": "0.1",
            "id": "ferridis.fs.v1",
            "name": "FS",
            "category": "files",
            "summary": "Filesystem.",
            "intents": [{ "verb": "read-file", "callers": "code" }, "list-dir"],
            "schema": { "type": "openapi-3", "url": "https://x/s.json" },
            "tiers": ["native"],
            "auth": { "type": "none" }
        }"#,
    )
    .expect("test manifest parses");
    let cap = CapabilityRef::parse("ferridis://public.ferridis.io/ferridis/fs@v1")
        .expect("test ref parses");
    let catalogue = Catalogue::from_sources(&[CatalogueSource::new(cap, manifest)]);
    ToolPlan::build(&catalogue, &CallerOverrides::default())
}

fn conversation(plan: &ToolPlan) -> Conversation {
    Conversation::new(
        ModelId::default_model(),
        MaxTokens::default(),
        plan.tool_set(),
    )
}

/// A paused response in the documented shape, one `tool_use` per call.
pub(crate) fn paused_response(calls: &[(&str, TestCaller)]) -> Value {
    let mut content = vec![
        json!({ "type": "text", "text": "Running code." }),
        json!({ "type": "server_tool_use", "id": "srvtoolu_p", "name": "code_execution",
                "input": { "code": "..." } }),
    ];
    content.extend(calls.iter().enumerate().map(|(i, (name, caller))| {
        json!({ "type": "tool_use", "id": format!("toolu_{i:03}"), "name": name,
                "input": { "path": "x" }, "caller": caller.json() })
    }));
    json!({
        "content": content,
        "stop_reason": "tool_use",
        "container": { "id": "container_p", "expires_at": "2099-01-01T00:00:00Z" },
        "usage": { "input_tokens": 1, "output_tokens": 1 }
    })
}

pub(crate) fn parse(response: &Value, plan: &ToolPlan) -> Turn {
    RawResponse::deserialize(response)
        .expect("documented shape decodes")
        .into_turn(conversation(plan))
        .expect("documented shape is a valid turn")
}

/// Records every dispatch; answers each with `{"ok": true}`.
#[derive(Debug, Default)]
struct Recording(Mutex<Vec<(CallOrigin, String)>>);

impl Recording {
    fn record(&self, origin: CallOrigin, intent: &IntentVerb) -> Value {
        self.0
            .lock()
            .expect("recording lock")
            .push((origin, intent.as_str().to_owned()));
        json!({ "ok": true })
    }

    fn dispatched(&self) -> Vec<(CallOrigin, String)> {
        std::mem::take(&mut *self.0.lock().expect("recording lock"))
    }
}

impl Dispatcher for Arc<Recording> {
    async fn request(
        &self,
        origin: CallOrigin,
        _capability: &CapabilityRef,
        intent: &IntentVerb,
        _body: Value,
    ) -> Result<Value, ClientError> {
        Ok(self.record(origin, intent))
    }

    async fn stream_collected(
        &self,
        origin: CallOrigin,
        _capability: &CapabilityRef,
        intent: &IntentVerb,
        _body: Value,
    ) -> Result<Value, ClientError> {
        Ok(self.record(origin, intent))
    }
}

/// Answers each call with content, or an error when its id ends in an
/// odd digit: both result shapes are exercised.
struct Alternating;

impl CallHandler for Alternating {
    fn handle<'a>(
        &'a self,
        call: &'a PendingCall,
    ) -> impl std::future::Future<Output = ToolOutput> + 'a {
        let odd = call.id().as_str().ends_with(['1', '3', '5', '7', '9']);
        async move {
            if odd {
                ToolOutput::Error("boom".to_owned())
            } else {
                ToolOutput::Content("ok".to_owned())
            }
        }
    }
}

/// Sync-to-async glue for the synchronous proptest bodies.
fn run_to_completion<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test runtime builds")
        .block_on(f) // allow:block-on — proptest bodies are synchronous; this is the top of the stack
}

/// The new message of a continuation body, and the one before it. (The
/// test conversations skip the opening prompt, so these may be all.)
fn last_two_messages(body: &Value) -> (&Value, &Value) {
    let messages = body["messages"].as_array().expect("messages is an array");
    let n = messages.len();
    assert!(n >= 2, "assistant, results: {body}");
    (&messages[n - 2], &messages[n - 1])
}

fn arb_caller() -> impl Strategy<Value = TestCaller> {
    prop_oneof![
        Just(TestCaller::Direct),
        Just(TestCaller::Code),
        Just(TestCaller::Unrecognised)
    ]
}

fn arb_tool() -> impl Strategy<Value = &'static str> {
    prop_oneof![Just(CODE_TOOL), Just(DIRECT_TOOL), Just(UNKNOWN_TOOL)]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Properties 1 and 2: one `tool_result` per pending id, nothing else
    /// in the message, and the assistant content resent verbatim.
    #[test]
    fn every_call_gets_exactly_one_result_and_nothing_else(
        calls in prop::collection::vec((arb_tool(), arb_caller()), 1..12)
    ) {
        let plan = plan();
        let response = paused_response(&calls);
        let Turn::Programmatic(pending) = parse(&response, &plan) else {
            panic!("a response with a container is programmatic");
        };
        let ids: Vec<String> = pending.calls().iter().map(|c| c.id().as_str().to_owned()).collect();
        let body = run_to_completion(pending.respond_with(&Alternating)).body_json();
        let (assistant, results) = last_two_messages(&body);

        prop_assert_eq!(&assistant["content"], &response["content"]);
        prop_assert_eq!(&results["role"], "user");
        let blocks = results["content"].as_array().expect("results content is an array");
        prop_assert!(blocks.iter().all(|b| b["type"] == "tool_result"), "{}", results);
        let mut answered: BTreeMap<&str, usize> = BTreeMap::new();
        for b in blocks {
            *answered.entry(b["tool_use_id"].as_str().expect("id is a string")).or_default() += 1;
        }
        prop_assert_eq!(answered.len(), ids.len());
        prop_assert!(ids.iter().all(|id| answered.get(id.as_str()) == Some(&1)));
        prop_assert_eq!(&body["container"], "container_p");
    }

    /// Property 3: the gate admits exactly the calls whose caller matches
    /// the tool's policy; every other call is an error result and the
    /// dispatcher never sees it.
    #[test]
    fn a_refused_call_never_reaches_the_dispatcher(
        calls in prop::collection::vec((arb_tool(), arb_caller()), 1..12)
    ) {
        let plan = plan();
        let Turn::Programmatic(pending) = parse(&paused_response(&calls), &plan) else {
            panic!("a response with a container is programmatic");
        };
        let recording = Arc::new(Recording::default());
        let executor = Executor::new(plan, Arc::clone(&recording));
        let body = run_to_completion(pending.respond_with(&executor)).body_json();
        let (_, results) = last_two_messages(&body);
        let blocks = results["content"].as_array().expect("results content is an array");

        let mut expected = Vec::new();
        for ((tool, caller), block) in calls.iter().zip(blocks) {
            let admitted = matches!(
                (*tool, caller),
                (CODE_TOOL, TestCaller::Code) | (DIRECT_TOOL, TestCaller::Direct)
            );
            prop_assert_eq!(block["is_error"] == true, !admitted, "{} from {:?}", tool, caller);
            if admitted {
                expected.push(match caller {
                    TestCaller::Code => (CallOrigin::ModelCode, "read-file".to_owned()),
                    _ => (CallOrigin::ModelDirect, "list-dir".to_owned()),
                });
            }
        }
        let dispatched = recording.dispatched();
        prop_assert_eq!(dispatched.len(), expected.len());
        prop_assert_eq!(executor.counts().refused() as usize, calls.len() - expected.len());
        for e in &expected {
            prop_assert!(dispatched.contains(e), "{:?} not dispatched", e);
        }
    }
}
