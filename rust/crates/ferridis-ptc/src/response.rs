//! What comes back: a parsed response, as the turn it puts the
//! conversation in.
//!
//! The turn types are a typestate. A response that stops for tool calls
//! yields [`PendingCalls`], whose only continuation is
//! [`PendingCalls::respond_with`]: it answers every pending call exactly
//! once, by construction, with a message that can hold nothing but
//! `tool_result` blocks.

use std::collections::HashSet;
use std::future::Future;

use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

use crate::client::PtcClient;
use crate::container::{Container, ContainerExpiry};
use crate::error::{PtcError, ResponseError};
use crate::ids::{ContainerId, ServerToolUseId, ToolUseId};
use crate::request::{Conversation, Message};

/// Token usage of one response, or summed over several.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct Usage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

impl Usage {
    /// Input tokens.
    #[must_use]
    pub fn input_tokens(&self) -> u64 {
        self.input_tokens
    }

    /// Output tokens, thinking included.
    #[must_use]
    pub fn output_tokens(&self) -> u64 {
        self.output_tokens
    }
}

impl std::ops::Add for Usage {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self {
            input_tokens: self.input_tokens + rhs.input_tokens,
            output_tokens: self.output_tokens + rhs.output_tokens,
        }
    }
}

/// Who made a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// The model, directly.
    Direct,
    /// Code running in the code-execution container.
    Code {
        /// The code-execution run that made the call.
        tool_id: ServerToolUseId,
    },
    /// A caller type this crate does not know. Never dispatched.
    Unrecognised {
        /// The raw `caller.type`.
        caller_type: String,
    },
}

impl std::fmt::Display for Caller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Direct => f.write_str("direct"),
            Self::Code { tool_id } => write!(f, "code ({tool_id})"),
            Self::Unrecognised { caller_type } => write!(f, "unrecognised ({caller_type})"),
        }
    }
}

/// One tool call awaiting its result.
#[derive(Debug, Clone)]
pub struct PendingCall {
    id: ToolUseId,
    name: String,
    input: Value,
    caller: Caller,
}

impl PendingCall {
    /// The `tool_use` id the result must answer.
    #[must_use]
    pub fn id(&self) -> &ToolUseId {
        &self.id
    }

    /// The tool name the model used. Not yet known to be registered.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The call's input.
    #[must_use]
    pub fn input(&self) -> &Value {
        &self.input
    }

    /// Who made the call.
    #[must_use]
    pub fn caller(&self) -> &Caller {
        &self.caller
    }
}

/// A tool call's result: content, or an error the model (or its code)
/// sees as `is_error: true`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolOutput {
    /// The call succeeded.
    Content(String),
    /// The call failed or was refused.
    Error(String),
}

impl ToolOutput {
    /// Whether this is an error result.
    #[must_use]
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }
}

#[derive(Debug)]
struct ToolResult {
    tool_use_id: ToolUseId,
    output: ToolOutput,
}

impl Serialize for ToolResult {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(None)?;
        map.serialize_entry("type", "tool_result")?;
        map.serialize_entry("tool_use_id", &self.tool_use_id)?;
        match &self.output {
            ToolOutput::Content(text) => map.serialize_entry("content", text)?,
            ToolOutput::Error(text) => {
                map.serialize_entry("content", text)?;
                map.serialize_entry("is_error", &true)?;
            }
        }
        map.end()
    }
}

/// The user message answering a set of calls. Holds only `tool_result`
/// blocks: the API rejects any other block alongside results for
/// programmatic calls, and this type has nowhere to put one.
#[derive(Debug)]
pub struct ToolResultsMessage(Vec<ToolResult>);

impl Serialize for ToolResultsMessage {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for r in &self.0 {
            seq.serialize_element(r)?;
        }
        seq.end()
    }
}

/// Answers each pending call.
pub trait CallHandler {
    /// The result for `call`.
    fn handle<'a>(&'a self, call: &'a PendingCall) -> impl Future<Output = ToolOutput> + 'a;
}

mod sealed {
    pub trait Sealed {}
}

/// Whether pending calls run in a container. Sealed: the two states below
/// are the only ones.
pub trait ContainerState: sealed::Sealed + std::fmt::Debug {
    /// The container, if any.
    fn container(&self) -> Option<&Container>;
}

/// Pending calls in a code-execution container: some may be programmatic.
#[derive(Debug)]
pub struct WithContainer(Container);

/// Pending calls with no container: every call is direct.
#[derive(Debug)]
pub struct NoContainer;

impl sealed::Sealed for WithContainer {}
impl sealed::Sealed for NoContainer {}

impl ContainerState for WithContainer {
    fn container(&self) -> Option<&Container> {
        Some(&self.0)
    }
}

impl ContainerState for NoContainer {
    fn container(&self) -> Option<&Container> {
        None
    }
}

/// Tool calls awaiting results. The conversation can only continue
/// through [`Self::respond_with`].
#[derive(Debug)]
pub struct PendingCalls<C: ContainerState> {
    conversation: Conversation,
    calls: Vec<PendingCall>,
    container: C,
    usage: Usage,
}

/// Calls paused in a code-execution container: the response that made
/// programmatic calls.
pub type PendingProgrammaticCalls = PendingCalls<WithContainer>;

/// Direct calls, no container.
pub type PendingDirectCalls = PendingCalls<NoContainer>;

impl<C: ContainerState> PendingCalls<C> {
    /// The calls, in response order. Never empty.
    #[must_use]
    pub fn calls(&self) -> &[PendingCall] {
        &self.calls
    }

    /// This response's token usage.
    #[must_use]
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// Answer every call with `handler`, concurrently, one result per
    /// call id.
    pub async fn respond_with<H: CallHandler>(self, handler: &H) -> AnsweredCalls<C> {
        let outputs =
            futures_util::future::join_all(self.calls.iter().map(|c| handler.handle(c))).await;
        let results = self
            .calls
            .into_iter()
            .zip(outputs)
            .map(|(call, output)| ToolResult {
                tool_use_id: call.id,
                output,
            })
            .collect();
        let mut conversation = self.conversation;
        conversation.push(Message::ToolResults(ToolResultsMessage(results)));
        AnsweredCalls {
            conversation,
            container: self.container,
        }
    }
}

impl PendingCalls<WithContainer> {
    /// The container the calls run in.
    #[must_use]
    pub fn container(&self) -> &Container {
        &self.container.0
    }
}

/// Every pending call answered; ready to send.
#[derive(Debug)]
pub struct AnsweredCalls<C: ContainerState> {
    conversation: Conversation,
    container: C,
}

impl<C: ContainerState> AnsweredCalls<C> {
    /// Send the results.
    ///
    /// # Errors
    ///
    /// [`PtcError::ContainerExpired`] before anything is sent if the
    /// container has expired; otherwise as [`PtcClient::start`].
    pub async fn send(self, client: &PtcClient) -> Result<Turn, PtcError> {
        let Self {
            conversation,
            container,
        } = self;
        client.exchange(conversation, container.container()).await
    }

    /// The request body [`Self::send`] would post.
    #[cfg(test)]
    pub(crate) fn body_json(&self) -> Value {
        serde_json::to_value(
            self.conversation
                .body(self.container.container().map(Container::id)),
        )
        .expect("request body serializes")
    }
}

/// The server paused a long-running turn; resending continues it.
#[derive(Debug)]
pub struct PausedTurn {
    conversation: Conversation,
    container: Option<Container>,
    usage: Usage,
}

impl PausedTurn {
    /// This response's token usage.
    #[must_use]
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// Continue the paused turn.
    ///
    /// # Errors
    ///
    /// As [`AnsweredCalls::send`].
    pub async fn resume(self, client: &PtcClient) -> Result<Turn, PtcError> {
        client
            .exchange(self.conversation, self.container.as_ref())
            .await
    }
}

/// Why a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The model finished.
    EndTurn,
    /// A stop sequence matched.
    StopSequence,
    /// The output cap was reached.
    MaxTokens,
    /// The model declined. Server-side refusal fallback is not enabled.
    Refusal,
    /// A stop reason this crate does not know.
    Other(String),
}

/// A finished code-execution run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeRun {
    /// The code ran.
    Finished {
        /// Standard output.
        stdout: String,
        /// Standard error.
        stderr: String,
        /// Exit status.
        return_code: i64,
    },
    /// The tool could not run the code.
    Failed {
        /// The API's error code.
        error_code: String,
    },
}

/// A turn that needs nothing more from the caller.
#[derive(Debug)]
pub struct FinalTurn {
    stop: StopReason,
    text: String,
    code_runs: Vec<CodeRun>,
    usage: Usage,
}

impl FinalTurn {
    /// Why it ended.
    #[must_use]
    pub fn stop_reason(&self) -> &StopReason {
        &self.stop
    }

    /// The response's text blocks, joined.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The code runs whose results this response reports.
    #[must_use]
    pub fn code_runs(&self) -> &[CodeRun] {
        &self.code_runs
    }

    /// This response's token usage.
    #[must_use]
    pub fn usage(&self) -> Usage {
        self.usage
    }
}

/// Where a response leaves the conversation.
#[derive(Debug)]
pub enum Turn {
    /// Finished.
    Final(FinalTurn),
    /// Calls to answer, in a container.
    Programmatic(PendingProgrammaticCalls),
    /// Direct calls to answer, no container.
    Direct(PendingDirectCalls),
    /// Paused by the server; resume to continue.
    Paused(PausedTurn),
}

impl Turn {
    /// This response's token usage.
    #[must_use]
    pub fn usage(&self) -> Usage {
        match self {
            Self::Final(t) => t.usage(),
            Self::Programmatic(t) => t.usage(),
            Self::Direct(t) => t.usage(),
            Self::Paused(t) => t.usage(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawResponse {
    content: Vec<Value>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    container: Option<RawContainer>,
    #[serde(default)]
    usage: Usage,
}

#[derive(Debug, Deserialize)]
struct RawContainer {
    id: String,
    expires_at: String,
}

#[derive(Debug, Deserialize)]
struct RawToolUse {
    id: String,
    name: String,
    #[serde(default)]
    input: Value,
    #[serde(default)]
    caller: Option<RawCaller>,
}

#[derive(Debug, Deserialize)]
struct RawCaller {
    #[serde(rename = "type")]
    caller_type: String,
    #[serde(default)]
    tool_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawCodeResultBlock {
    content: RawCodeResult,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum RawCodeResult {
    #[serde(rename = "code_execution_result")]
    Result {
        #[serde(default)]
        stdout: String,
        #[serde(default)]
        stderr: String,
        #[serde(default)]
        return_code: i64,
    },
    #[serde(rename = "code_execution_tool_result_error")]
    Error { error_code: String },
}

fn parse_caller(raw: Option<RawCaller>) -> Result<Caller, ResponseError> {
    let Some(raw) = raw else {
        return Ok(Caller::Direct);
    };
    match (raw.caller_type.as_str(), raw.tool_id) {
        ("direct", _) => Ok(Caller::Direct),
        (t, Some(tool_id)) if t.starts_with("code_execution_") => Ok(Caller::Code {
            tool_id: ServerToolUseId::parse("caller.tool_id", tool_id)?,
        }),
        _ => Ok(Caller::Unrecognised {
            caller_type: raw.caller_type,
        }),
    }
}

enum StopReasonOrPause {
    ToolUse,
    Pause,
    Stop(StopReason),
}

fn parse_stop(raw: Option<String>) -> StopReasonOrPause {
    match raw.as_deref() {
        Some("tool_use") => StopReasonOrPause::ToolUse,
        Some("pause_turn") => StopReasonOrPause::Pause,
        Some("end_turn") => StopReasonOrPause::Stop(StopReason::EndTurn),
        Some("stop_sequence") => StopReasonOrPause::Stop(StopReason::StopSequence),
        Some("max_tokens") => StopReasonOrPause::Stop(StopReason::MaxTokens),
        Some("refusal") => StopReasonOrPause::Stop(StopReason::Refusal),
        _ => StopReasonOrPause::Stop(StopReason::Other(raw.unwrap_or_default())),
    }
}

/// The typed view of a response's content.
struct Blocks {
    text: String,
    calls: Vec<PendingCall>,
    code_runs: Vec<CodeRun>,
}

fn parse_tool_use(block: &Value) -> Result<PendingCall, ResponseError> {
    let raw = RawToolUse::deserialize(block).map_err(|source| ResponseError::MalformedBlock {
        block_type: "tool_use",
        source,
    })?;
    Ok(PendingCall {
        id: ToolUseId::parse("tool_use.id", raw.id)?,
        name: raw.name,
        input: raw.input,
        caller: parse_caller(raw.caller)?,
    })
}

fn parse_code_run(block: &Value) -> Option<CodeRun> {
    match RawCodeResultBlock::deserialize(block) {
        Ok(RawCodeResultBlock {
            content:
                RawCodeResult::Result {
                    stdout,
                    stderr,
                    return_code,
                },
        }) => Some(CodeRun::Finished {
            stdout,
            stderr,
            return_code,
        }),
        Ok(RawCodeResultBlock {
            content: RawCodeResult::Error { error_code },
        }) => Some(CodeRun::Failed { error_code }),
        // Reporting only: an unknown result shape must not discard a
        // finished answer, so it is logged and left out.
        Err(e) => {
            tracing::warn!(error = %e, "unrecognised code_execution_tool_result; not reported");
            None
        }
    }
}

fn parse_blocks(content: &[Value]) -> Result<Blocks, ResponseError> {
    let mut blocks = Blocks {
        text: String::new(),
        calls: Vec::new(),
        code_runs: Vec::new(),
    };
    let mut seen: HashSet<&str> = HashSet::new();
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    blocks.text.push_str(t);
                }
            }
            Some("tool_use") => {
                let call = parse_tool_use(block)?;
                let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                if !seen.insert(id) {
                    return Err(ResponseError::DuplicateToolUseId { id: id.to_owned() });
                }
                blocks.calls.push(call);
            }
            Some("code_execution_tool_result") => {
                blocks.code_runs.extend(parse_code_run(block));
            }
            _ => {}
        }
    }
    Ok(blocks)
}

impl RawResponse {
    /// Append this response to `conversation` and say where that leaves it.
    pub(crate) fn into_turn(self, mut conversation: Conversation) -> Result<Turn, ResponseError> {
        let blocks = parse_blocks(&self.content)?;
        let container = self
            .container
            .map(|c| {
                Ok::<_, ResponseError>(Container::new(
                    ContainerId::parse("container.id", c.id)?,
                    ContainerExpiry::parse(&c.expires_at)?,
                ))
            })
            .transpose()?;
        let usage = self.usage;
        conversation.push(Message::Assistant(self.content));
        let turn = match parse_stop(self.stop_reason) {
            StopReasonOrPause::ToolUse => pending(conversation, blocks.calls, container, usage)?,
            StopReasonOrPause::Pause => Turn::Paused(PausedTurn {
                conversation,
                container,
                usage,
            }),
            StopReasonOrPause::Stop(stop) => Turn::Final(FinalTurn {
                stop,
                text: blocks.text,
                code_runs: blocks.code_runs,
                usage,
            }),
        };
        Ok(turn)
    }
}

/// The pending-calls turn for a `tool_use` stop. A container makes it
/// programmatic; a call from code without one is a contract violation.
fn pending(
    conversation: Conversation,
    calls: Vec<PendingCall>,
    container: Option<Container>,
    usage: Usage,
) -> Result<Turn, ResponseError> {
    if calls.is_empty() {
        return Err(ResponseError::ToolUseWithoutCalls);
    }
    if let Some(c) = container {
        return Ok(Turn::Programmatic(PendingCalls {
            conversation,
            calls,
            container: WithContainer(c),
            usage,
        }));
    }
    if let Some(call) = calls
        .iter()
        .find(|c| matches!(c.caller, Caller::Code { .. }))
    {
        return Err(ResponseError::ProgrammaticCallWithoutContainer {
            tool_use_id: call.id.as_str().to_owned(),
        });
    }
    Ok(Turn::Direct(PendingCalls {
        conversation,
        calls,
        container: NoContainer,
        usage,
    }))
}
