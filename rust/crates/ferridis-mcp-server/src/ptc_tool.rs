//! `ferridis_ptc_run`: programmatic tool calling, offered to MCP clients
//! as one tool.
//!
//! An MCP client calls tools one at a time; programmatic tool calling
//! needs a host that talks to the Messages API itself. This tool is that
//! host: a client hands it a task, and the server runs one
//! `ferridis-ptc` conversation over the same registered tools it
//! publishes, then returns the final answer. Claude calls the
//! code-callable tools from code in its sandbox, so their intermediate
//! results never reach the MCP client's context either.
//!
//! The tool exists only when `--ptc-api-key-file` is given. Every call is
//! billed to that key, so runs are bounded: [`MaxTurns`] per run, and one
//! run at a time per server.

use std::sync::Arc;

use ferridis_client::adapters::CallerOverrides;
use ferridis_client::catalogue::Catalogue;
use ferridis_client::{CallOrigin, Client, ClientError};
use ferridis_core::{CallerPolicy, CapabilityRef, IntentVerb};
use ferridis_ptc::{
    ApiKey, Conversation, Dispatcher, Executor, MaxTokens, MaxTurns, ModelId, ParameterError,
    PtcClient, PtcError, RunOutcome, StopReason, ToolPlan, ToolSet, UserPrompt, WorkspaceId, run,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};
use url::Url;

use crate::protocol::{ContentPart, Tool, ToolsCallResult};

/// The tool's MCP name. Startup refuses a catalogue tool of the same name
/// rather than letting one shadow the other.
pub(crate) const PTC_TOOL_NAME: &str = "ferridis_ptc_run";

/// What the `--ptc-*` flags configured.
#[derive(Debug)]
pub(crate) struct PtcSettings {
    api_key: ApiKey,
    workspace: Option<WorkspaceId>,
    endpoint: Option<Url>,
}

impl PtcSettings {
    /// `endpoint` replaces the public Messages API, for a proxy or a
    /// replay server in tests.
    pub(crate) fn new(
        api_key: ApiKey,
        workspace: Option<WorkspaceId>,
        endpoint: Option<Url>,
    ) -> Self {
        Self {
            api_key,
            workspace,
            endpoint,
        }
    }
}

/// The publisher's client, shared with the PTC executor. A local type, so
/// it can carry `ferridis-ptc`'s [`Dispatcher`]; every call forwards to
/// `Client`'s own implementation, which records the caller on the
/// tier-used telemetry.
struct SharedClient(Arc<Client>);

/// `Client` has no `Debug`; the executor's needs only a name.
impl std::fmt::Debug for SharedClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SharedClient")
    }
}

impl Dispatcher for SharedClient {
    async fn request(
        &self,
        origin: CallOrigin,
        capability: &CapabilityRef,
        intent: &IntentVerb,
        body: Value,
    ) -> Result<Value, ClientError> {
        Dispatcher::request(self.0.as_ref(), origin, capability, intent, body).await
    }

    async fn stream_collected(
        &self,
        origin: CallOrigin,
        capability: &CapabilityRef,
        intent: &IntentVerb,
        body: Value,
    ) -> Result<Value, ClientError> {
        Dispatcher::stream_collected(self.0.as_ref(), origin, capability, intent, body).await
    }
}

/// The tool, ready to answer `tools/call`.
#[derive(Debug)]
pub(crate) struct PtcTool {
    ptc: PtcClient,
    executor: Executor<SharedClient>,
    tools: Arc<ToolSet>,
    max_turns: MaxTurns,
    descriptor: Tool,
    /// One run at a time. A second call waits for the first to finish:
    /// this bounds what concurrent calls can spend, and keeps each run's
    /// call counts exact (the executor's counters are shared).
    gate: tokio::sync::Mutex<()>,
}

impl PtcTool {
    /// Plan the catalogue's tools under `overrides`, the same caller
    /// policy `ptc_run` uses for the same adapters file.
    pub(crate) fn new(
        settings: PtcSettings,
        client: Arc<Client>,
        catalogue: &Catalogue,
        overrides: &CallerOverrides,
    ) -> Self {
        let plan = ToolPlan::build(catalogue, overrides);
        let code = plan
            .tools()
            .iter()
            .filter(|t| t.policy().policy() == CallerPolicy::Code)
            .count();
        let direct = plan.tools().len() - code;
        let max_turns = MaxTurns::default();
        let model = ModelId::default_model();
        let descriptor = descriptor(model.as_str(), code, direct, max_turns);
        info!(
            tool = PTC_TOOL_NAME,
            code_callable = code,
            direct_only = direct,
            "programmatic tool calling offered"
        );
        if code == 0 {
            warn!(
                tool = PTC_TOOL_NAME,
                "no tool is code-callable: runs will call every tool directly, one model turn \
                 per call. Mark read-only tools \"code\" in the adapters file's `callers` map"
            );
        }

        let mut ptc = PtcClient::new(settings.api_key);
        if let Some(w) = settings.workspace {
            ptc = ptc.with_workspace(w);
        }
        if let Some(e) = settings.endpoint {
            ptc = ptc.with_endpoint(e);
        }
        let tools = plan.tool_set();
        Self {
            ptc,
            executor: Executor::new(plan, SharedClient(client)),
            tools,
            max_turns,
            descriptor,
            gate: tokio::sync::Mutex::new(()),
        }
    }

    /// The `tools/list` entry.
    pub(crate) fn descriptor(&self) -> &Tool {
        &self.descriptor
    }

    /// Answer one `tools/call`. Failures are tool errors (`isError`), so
    /// the calling model sees them, as for every other tool.
    pub(crate) async fn call(&self, arguments: Value) -> ToolsCallResult {
        match self.run(arguments).await {
            Ok(outcome) => answer(&outcome),
            Err(e) => {
                warn!(tool = PTC_TOOL_NAME, error = %e, "programmatic run failed");
                ToolsCallResult {
                    content: vec![ContentPart::Text {
                        text: format!("{PTC_TOOL_NAME} failed: {e}"),
                    }],
                    is_error: Some(true),
                }
            }
        }
    }

    async fn run(&self, arguments: Value) -> Result<RunOutcome, PtcCallError> {
        let task = parse_task(arguments)?;
        let _one_at_a_time = self.gate.lock().await;
        info!(
            tool = PTC_TOOL_NAME,
            task_chars = task.as_str().chars().count(),
            "programmatic run started"
        );
        let conversation = Conversation::new(
            ModelId::default_model(),
            MaxTokens::default(),
            Arc::clone(&self.tools),
        );
        let outcome = run(
            &self.ptc,
            &self.executor,
            conversation,
            task,
            self.max_turns,
        )
        .await
        .map_err(PtcCallError::Run)?;
        let usage = outcome.usage();
        let calls = outcome.calls();
        info!(
            tool = PTC_TOOL_NAME,
            turns = outcome.turns(),
            programmatic = calls.programmatic(),
            direct = calls.direct(),
            refused = calls.refused(),
            failed = calls.failed(),
            input_tokens = usage.input_tokens(),
            cache_write_tokens = usage.cache_creation_input_tokens(),
            cache_read_tokens = usage.cache_read_input_tokens(),
            output_tokens = usage.output_tokens(),
            "programmatic run finished"
        );
        Ok(outcome)
    }
}

/// Why a call produced no answer.
#[derive(Debug, thiserror::Error)]
enum PtcCallError {
    #[error("arguments must be {{\"task\": \"<what to do>\"}}: {0}")]
    Arguments(#[source] serde_json::Error),

    #[error("invalid task: {0}")]
    Task(#[source] ParameterError),

    #[error("{0}")]
    Run(#[source] PtcError),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PtcArguments {
    task: String,
}

fn parse_task(arguments: Value) -> Result<UserPrompt, PtcCallError> {
    let args: PtcArguments = serde_json::from_value(arguments).map_err(PtcCallError::Arguments)?;
    UserPrompt::new(args.task).map_err(PtcCallError::Task)
}

/// The final answer, then one line saying what the run did and spent.
/// A refusal is an error result; a run cut off by the output cap is an
/// answer that says so.
fn answer(outcome: &RunOutcome) -> ToolsCallResult {
    let final_turn = outcome.final_turn();
    let text = final_turn.text().trim();
    let (body, is_error) = match final_turn.stop_reason() {
        StopReason::Refusal => (format!("Claude declined the task. {text}"), Some(true)),
        StopReason::MaxTokens => (
            format!("{text}\n\n(The answer was cut off at the output limit.)"),
            None,
        ),
        StopReason::EndTurn | StopReason::StopSequence | StopReason::Other(_) => {
            (text.to_owned(), None)
        }
    };
    let calls = outcome.calls();
    let usage = outcome.usage();
    let summary = format!(
        "{PTC_TOOL_NAME}: {turns} turns; {programmatic} calls from code, {direct} direct, \
         {refused} refused, {failed} failed; input tokens {input} uncached, {write} cache \
         write, {read} cache read; output tokens {output}",
        turns = outcome.turns(),
        programmatic = calls.programmatic(),
        direct = calls.direct(),
        refused = calls.refused(),
        failed = calls.failed(),
        input = usage.input_tokens(),
        write = usage.cache_creation_input_tokens(),
        read = usage.cache_read_input_tokens(),
        output = usage.output_tokens(),
    );
    ToolsCallResult {
        content: vec![ContentPart::Text {
            text: format!("{body}\n\n---\n{summary}"),
        }],
        is_error,
    }
}

fn descriptor(model: &str, code: usize, direct: usize, max_turns: MaxTurns) -> Tool {
    Tool {
        name: PTC_TOOL_NAME.to_owned(),
        description: format!(
            "Run a task as one programmatic-tool-calling conversation with Claude ({model}) over \
             the Anthropic Messages API. Claude can call {code} of this server's tools from code \
             in a sandbox, looping and filtering there, and {direct} more directly; only the \
             final answer comes back. Use it for work that needs many tool calls or large \
             intermediate results. Each call is billed to the Anthropic API key configured on \
             the server, runs at most {turns} model turns, and calls run one at a time.",
            turns = max_turns.get(),
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "minLength": 1,
                    "description": "What to do, in plain language. Claude plans the tool calls."
                }
            },
            "required": ["task"],
            "additionalProperties": false
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_parses_and_anything_else_is_refused() {
        let ok = parse_task(json!({ "task": "count the files" })).expect("task parses");
        assert_eq!(ok.as_str(), "count the files");

        for bad in [
            json!(null),
            json!({}),
            json!({ "task": 3 }),
            json!({ "task": "x", "extra": true }),
        ] {
            assert!(
                matches!(parse_task(bad.clone()), Err(PtcCallError::Arguments(_))), // allow:clone — the value is printed if the assertion fails
                "{bad}"
            );
        }
        assert!(matches!(
            parse_task(json!({ "task": "   " })),
            Err(PtcCallError::Task(_))
        ));
    }

    #[test]
    fn the_descriptor_states_cost_bounds_and_requires_a_task() {
        let tool = descriptor("claude-opus-5-5", 3, 4, MaxTurns::default());
        assert_eq!(tool.name, PTC_TOOL_NAME);
        assert!(tool.description.contains("billed"), "{}", tool.description);
        assert!(
            tool.description
                .contains("3 of this server's tools from code")
        );
        assert_eq!(tool.input_schema["required"], json!(["task"]));
        assert_eq!(tool.input_schema["additionalProperties"], json!(false));
    }
}
