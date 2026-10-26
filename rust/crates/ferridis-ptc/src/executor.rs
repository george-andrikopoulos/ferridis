//! Answering calls: the caller policy gate, then dispatch through the
//! Ferridis client.

use std::collections::HashMap;
use std::future::Future;
use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use ferridis_client::catalogue::ToolName;
use ferridis_client::{CallOrigin, Client, ClientError};
use ferridis_core::{CallerPolicy, CapabilityRef, IntentKind, IntentVerb};
use serde_json::Value;

use crate::client::PtcClient;
use crate::error::{ParameterError, PtcError};
use crate::request::{Conversation, UserPrompt};
use crate::response::{CallHandler, Caller, FinalTurn, PendingCall, ToolOutput, Turn, Usage};
use crate::tools::{PlannedTool, ToolPlan};

/// Where an allowed call goes. Implemented for [`Client`]; a host with its
/// own dispatch layer implements it too.
pub trait Dispatcher {
    /// Dispatch a request-kind intent.
    fn request(
        &self,
        origin: CallOrigin,
        capability: &CapabilityRef,
        intent: &IntentVerb,
        body: Value,
    ) -> impl Future<Output = Result<Value, ClientError>>;

    /// Dispatch a stream-kind intent and collect it into one value.
    fn stream_collected(
        &self,
        origin: CallOrigin,
        capability: &CapabilityRef,
        intent: &IntentVerb,
        body: Value,
    ) -> impl Future<Output = Result<Value, ClientError>>;
}

impl Dispatcher for Client {
    async fn request(
        &self,
        origin: CallOrigin,
        capability: &CapabilityRef,
        intent: &IntentVerb,
        body: Value,
    ) -> Result<Value, ClientError> {
        self.dispatch_from(origin, capability, intent.clone(), body) // allow:clone — dispatch takes the verb by value; the plan keeps its own
            .await
    }

    async fn stream_collected(
        &self,
        origin: CallOrigin,
        capability: &CapabilityRef,
        intent: &IntentVerb,
        body: Value,
    ) -> Result<Value, ClientError> {
        let stream = self
            .dispatch_streaming_from(origin, capability, intent.clone(), body) // allow:clone — same
            .await?;
        Ok(ferridis_client::catalogue::collect_stream(stream)
            .await?
            .into_json())
    }
}

/// What happened to the calls an executor answered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallCounts {
    programmatic: u32,
    direct: u32,
    refused: u32,
    failed: u32,
    oversized: u32,
    timed_out: u32,
}

impl CallCounts {
    /// Calls from code that reached their adapter.
    #[must_use]
    pub fn programmatic(&self) -> u32 {
        self.programmatic
    }

    /// Direct calls that reached their adapter.
    #[must_use]
    pub fn direct(&self) -> u32 {
        self.direct
    }

    /// Calls refused before dispatch: unknown tool, or a caller the tool's
    /// policy does not allow.
    #[must_use]
    pub fn refused(&self) -> u32 {
        self.refused
    }

    /// Dispatched calls whose adapter returned an error.
    #[must_use]
    pub fn failed(&self) -> u32 {
        self.failed
    }

    /// Dispatched calls whose result exceeded [`MaxResultBytes`] and was
    /// replaced by an error result.
    #[must_use]
    pub fn oversized(&self) -> u32 {
        self.oversized
    }

    /// Dispatched calls abandoned at their [`CallDeadline`]. The adapter
    /// may still have acted on them.
    #[must_use]
    pub fn timed_out(&self) -> u32 {
        self.timed_out
    }
}

impl std::ops::Sub for CallCounts {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self {
            programmatic: self.programmatic - rhs.programmatic,
            direct: self.direct - rhs.direct,
            refused: self.refused - rhs.refused,
            failed: self.failed - rhs.failed,
            oversized: self.oversized - rhs.oversized,
            timed_out: self.timed_out - rhs.timed_out,
        }
    }
}

#[derive(Debug, Default)]
struct Counters {
    programmatic: AtomicU32,
    direct: AtomicU32,
    refused: AtomicU32,
    failed: AtomicU32,
    oversized: AtomicU32,
    timed_out: AtomicU32,
}

impl Counters {
    fn snapshot(&self) -> CallCounts {
        CallCounts {
            programmatic: self.programmatic.load(Ordering::Relaxed),
            direct: self.direct.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            oversized: self.oversized.load(Ordering::Relaxed),
            timed_out: self.timed_out.load(Ordering::Relaxed),
        }
    }
}

/// The largest tool result, in bytes of serialized JSON, returned to the
/// API. A larger one becomes an error result naming its size, never a
/// truncation: cut JSON would not parse in the code that asked for it.
///
/// It bounds what is sent, not what the adapter returns: the dispatch
/// has already read the whole response into memory when it is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxResultBytes(NonZeroUsize);

impl MaxResultBytes {
    /// Accept a non-zero limit.
    ///
    /// # Errors
    ///
    /// [`ParameterError::ZeroResultLimit`].
    pub fn new(bytes: usize) -> Result<Self, ParameterError> {
        NonZeroUsize::new(bytes)
            .map(Self)
            .ok_or(ParameterError::ZeroResultLimit)
    }

    /// The limit.
    #[must_use]
    pub fn get(self) -> usize {
        self.0.get()
    }

    /// The default for results going to `caller`.
    ///
    /// - `Direct`: 128 KiB, about 32K tokens the model reads, re-sent with
    ///   the history on every later turn.
    /// - `Code`: 1 MiB. Results returned to code are not model tokens, and
    ///   keeping large intermediate data out of the model's context is
    ///   what programmatic calling is for, but they still travel in every
    ///   later request, which the API caps in size.
    #[must_use]
    pub fn default_for(caller: CallerPolicy) -> Self {
        let bytes = match caller {
            CallerPolicy::Direct => 128 * 1024,
            CallerPolicy::Code => 1024 * 1024,
        };
        Self(NonZeroUsize::new(bytes).expect("both defaults are non-zero"))
    }
}

/// How long one dispatched call may take before it is abandoned and
/// answered with an error result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallDeadline(Duration);

impl CallDeadline {
    /// Accept a non-zero deadline.
    ///
    /// # Errors
    ///
    /// [`ParameterError::ZeroCallDeadline`].
    pub fn new(deadline: Duration) -> Result<Self, ParameterError> {
        if deadline.is_zero() {
            return Err(ParameterError::ZeroCallDeadline);
        }
        Ok(Self(deadline))
    }

    /// The deadline.
    #[must_use]
    pub fn get(self) -> Duration {
        self.0
    }
}

impl Default for CallDeadline {
    /// 180 s: under the API's roughly four-minute limit on a paused
    /// programmatic call, so the code gets an error result it can handle
    /// instead of the container giving up on it.
    fn default() -> Self {
        Self(Duration::from_secs(180))
    }
}

/// Why a call was refused before dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Refusal {
    UnknownTool,
    CallerNotAllowed { policy: CallerPolicy },
}

/// Answers calls against a [`ToolPlan`]: unknown tools and callers the
/// tool's policy does not allow get an error result and never reach a
/// dispatcher; everything else is dispatched with its origin recorded,
/// bounded by a [`CallDeadline`], and its result by [`MaxResultBytes`].
///
/// The deadline runs on the tokio timer, so answering calls needs a
/// runtime with time enabled (`#[tokio::main]` enables it).
#[derive(Debug)]
pub struct Executor<D> {
    tools: HashMap<ToolName, PlannedTool>,
    dispatcher: D,
    counters: Counters,
    max_direct_result: MaxResultBytes,
    max_code_result: MaxResultBytes,
    deadline: CallDeadline,
}

impl<D: Dispatcher> Executor<D> {
    /// An executor for `plan`'s tools.
    #[must_use]
    pub fn new(plan: ToolPlan, dispatcher: D) -> Self {
        let tools = plan
            .into_tools()
            .into_iter()
            .map(|t| (t.name().clone(), t)) // allow:clone — the index key and the tool each own the name
            .collect();
        Self {
            tools,
            dispatcher,
            counters: Counters::default(),
            max_direct_result: MaxResultBytes::default_for(CallerPolicy::Direct),
            max_code_result: MaxResultBytes::default_for(CallerPolicy::Code),
            deadline: CallDeadline::default(),
        }
    }

    /// Cap results going to `caller` at `limit` instead of
    /// [`MaxResultBytes::default_for`].
    #[must_use]
    pub fn with_max_result(mut self, caller: CallerPolicy, limit: MaxResultBytes) -> Self {
        match caller {
            CallerPolicy::Direct => self.max_direct_result = limit,
            CallerPolicy::Code => self.max_code_result = limit,
        }
        self
    }

    /// Abandon each call at `deadline` instead of the default.
    #[must_use]
    pub fn with_deadline(mut self, deadline: CallDeadline) -> Self {
        self.deadline = deadline;
        self
    }

    /// Cumulative counts over every call this executor answered.
    #[must_use]
    pub fn counts(&self) -> CallCounts {
        self.counters.snapshot()
    }

    /// The tool `call` names, if its caller is allowed to call it.
    fn admit(&self, call: &PendingCall) -> Result<(&PlannedTool, CallOrigin), Refusal> {
        let tool = self.tools.get(call.name()).ok_or(Refusal::UnknownTool)?;
        let policy = tool.policy().policy();
        let origin = match (policy, call.caller()) {
            (CallerPolicy::Direct, Caller::Direct) => CallOrigin::ModelDirect,
            (CallerPolicy::Code, Caller::Code { .. }) => CallOrigin::ModelCode,
            _ => return Err(Refusal::CallerNotAllowed { policy }),
        };
        Ok((tool, origin))
    }

    async fn execute(&self, call: &PendingCall) -> ToolOutput {
        let (tool, origin) = match self.admit(call) {
            Ok(admitted) => admitted,
            Err(refusal) => return self.refuse(call, &refusal),
        };
        let body = call.input().clone(); // allow:clone — dispatch consumes the body; the call stays in the history
        let dispatched = async {
            match tool.kind() {
                IntentKind::Request => {
                    self.dispatcher
                        .request(origin, tool.capability(), tool.intent(), body)
                        .await
                }
                IntentKind::Stream => {
                    self.dispatcher
                        .stream_collected(origin, tool.capability(), tool.intent(), body)
                        .await
                }
            }
        };
        let result = tokio::time::timeout(self.deadline.get(), dispatched).await;
        let counter = match origin {
            CallOrigin::ModelCode => &self.counters.programmatic,
            _ => &self.counters.direct,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        match result {
            Ok(Ok(value)) => self.bounded(call, origin, value.to_string()),
            Ok(Err(e)) => {
                self.counters.failed.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(tool = call.name(), caller = %call.caller(), error = %e, "tool call failed");
                ToolOutput::Error(format!("tool {} failed: {e}", call.name()))
            }
            Err(_elapsed) => {
                self.counters.timed_out.fetch_add(1, Ordering::Relaxed);
                let deadline = self.deadline.get();
                tracing::warn!(tool = call.name(), caller = %call.caller(), ?deadline,
                    "tool call abandoned at its deadline");
                ToolOutput::Error(format!(
                    "tool {} did not answer within {deadline:?} and was abandoned; \
                     it may still have taken effect",
                    call.name()
                ))
            }
        }
    }

    /// `text` as the result, or an error result when it exceeds the cap
    /// for results going back to `origin`.
    fn bounded(&self, call: &PendingCall, origin: CallOrigin, text: String) -> ToolOutput {
        let limit = match origin {
            CallOrigin::ModelCode => self.max_code_result,
            _ => self.max_direct_result,
        }
        .get();
        if text.len() <= limit {
            return ToolOutput::Content(text);
        }
        self.counters.oversized.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(tool = call.name(), caller = %call.caller(), bytes = text.len(), limit,
            "tool result over the size limit; replaced by an error result");
        ToolOutput::Error(format!(
            "tool {} returned {} bytes, over the {limit}-byte result limit; \
             ask for less (a narrower path, a smaller range)",
            call.name(),
            text.len()
        ))
    }

    fn refuse(&self, call: &PendingCall, refusal: &Refusal) -> ToolOutput {
        self.counters.refused.fetch_add(1, Ordering::Relaxed);
        let message = match refusal {
            Refusal::UnknownTool => format!("no tool named {} is registered", call.name()),
            Refusal::CallerNotAllowed { policy } => format!(
                "tool {} may only be called {}; refused a call from {}",
                call.name(),
                match policy {
                    CallerPolicy::Direct => "directly by the model",
                    CallerPolicy::Code => "from code in the code-execution container",
                },
                call.caller()
            ),
        };
        tracing::warn!(tool = call.name(), caller = %call.caller(), "{message}");
        ToolOutput::Error(message)
    }
}

impl<D: Dispatcher> CallHandler for Executor<D> {
    fn handle<'a>(&'a self, call: &'a PendingCall) -> impl Future<Output = ToolOutput> + 'a {
        self.execute(call)
    }
}

/// The most turns a run takes before giving up, so a model that keeps
/// calling tools cannot spend without bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaxTurns(NonZeroU32);

impl MaxTurns {
    /// Accept a non-zero limit.
    ///
    /// # Errors
    ///
    /// [`ParameterError::ZeroMaxTurns`].
    pub fn new(n: u32) -> Result<Self, ParameterError> {
        NonZeroU32::new(n)
            .map(Self)
            .ok_or(ParameterError::ZeroMaxTurns)
    }

    /// The limit.
    #[must_use]
    pub fn get(self) -> u32 {
        self.0.get()
    }
}

impl Default for MaxTurns {
    fn default() -> Self {
        Self(NonZeroU32::new(16).expect("16 is non-zero"))
    }
}

/// A finished run.
#[derive(Debug)]
pub struct RunOutcome {
    final_turn: FinalTurn,
    turns: u32,
    usage: Usage,
    calls: CallCounts,
}

impl RunOutcome {
    /// The last turn.
    #[must_use]
    pub fn final_turn(&self) -> &FinalTurn {
        &self.final_turn
    }

    /// How many responses the run took.
    #[must_use]
    pub fn turns(&self) -> u32 {
        self.turns
    }

    /// Token usage summed over every response.
    #[must_use]
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// What happened to the calls the executor answered while this run was
    /// in progress. Exact for an executor used by one run at a time; runs
    /// sharing an executor concurrently count each other's calls too.
    #[must_use]
    pub fn calls(&self) -> CallCounts {
        self.calls
    }
}

/// Drive `conversation` from `prompt` to a final turn, answering every
/// call with `executor` and resuming every pause.
///
/// # Errors
///
/// [`PtcError::TurnLimit`] if no final turn arrives within `limit`
/// responses; otherwise whatever the exchange returned.
pub async fn run<D: Dispatcher>(
    client: &PtcClient,
    executor: &Executor<D>,
    conversation: Conversation,
    prompt: UserPrompt,
    limit: MaxTurns,
) -> Result<RunOutcome, PtcError> {
    let before = executor.counts();
    let mut turn = client.start(conversation, prompt).await?;
    let mut turns = 1;
    let mut usage = turn.usage();
    loop {
        turn = match turn {
            Turn::Final(final_turn) => {
                return Ok(RunOutcome {
                    final_turn,
                    turns,
                    usage,
                    calls: executor.counts() - before,
                });
            }
            _ if turns >= limit.get() => return Err(PtcError::TurnLimit { limit: limit.get() }),
            Turn::Programmatic(p) => p.respond_with(executor).await.send(client).await?,
            Turn::Direct(p) => p.respond_with(executor).await.send(client).await?,
            Turn::Paused(p) => p.resume(client).await?,
        };
        turns += 1;
        usage = usage + turn.usage();
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::properties::{CODE_TOOL, TestCaller, parse, paused_response, plan};

    /// An adapter that answers an hour late.
    struct Stalled;

    impl Stalled {
        async fn late() -> Result<Value, ClientError> {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(json!({ "late": true }))
        }
    }

    impl Dispatcher for Stalled {
        async fn request(
            &self,
            _origin: CallOrigin,
            _capability: &CapabilityRef,
            _intent: &IntentVerb,
            _body: Value,
        ) -> Result<Value, ClientError> {
            Self::late().await
        }

        async fn stream_collected(
            &self,
            _origin: CallOrigin,
            _capability: &CapabilityRef,
            _intent: &IntentVerb,
            _body: Value,
        ) -> Result<Value, ClientError> {
            Self::late().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_call_that_outlives_its_deadline_is_answered_with_an_error() {
        let plan = plan();
        let Turn::Programmatic(pending) =
            parse(&paused_response(&[(CODE_TOOL, TestCaller::Code)]), &plan)
        else {
            panic!("a response with a container is programmatic");
        };
        let deadline = CallDeadline::new(Duration::from_secs(5)).expect("non-zero deadline");
        let executor = Executor::new(plan, Stalled).with_deadline(deadline);
        let started = tokio::time::Instant::now();

        let body = pending.respond_with(&executor).await.body_json();

        assert_eq!(started.elapsed(), Duration::from_secs(5));
        let result = &body["messages"]
            .as_array()
            .and_then(|m| m.last())
            .expect("a results message")["content"][0];
        assert_eq!(result["is_error"], true, "{result}");
        assert!(
            result["content"]
                .as_str()
                .is_some_and(|t| t.contains("may still have taken effect")),
            "{result}"
        );
        let counts = executor.counts();
        assert_eq!((counts.timed_out(), counts.programmatic()), (1, 1));
    }

    #[test]
    fn rejects_a_zero_deadline_and_a_zero_result_limit() {
        assert_eq!(
            CallDeadline::new(Duration::ZERO),
            Err(ParameterError::ZeroCallDeadline)
        );
        assert_eq!(MaxResultBytes::new(0), Err(ParameterError::ZeroResultLimit));
    }
}
